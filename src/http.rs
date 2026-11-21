use crate::runtime::{Budget, unix_time};
use anyhow::Result;
use reqwest::{
    StatusCode,
    blocking::{Client, RequestBuilder},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HTTP_ATTEMPTS: u8 = 3;

pub struct Services {
    pub client: Client,
    telegram_base: String,
    gemini_base: String,
    groq_base: String,
    pub groq_url: String,
}

impl Services {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            telegram_base: "https://api.telegram.org".into(),
            gemini_base: "https://generativelanguage.googleapis.com/v1beta/models".into(),
            groq_base: "https://api.groq.com/openai/v1".into(),
            groq_url: "https://api.groq.com/openai/v1/chat/completions".into(),
        })
    }

    pub fn telegram_url(&self, token: &str, method: &str) -> String {
        format!("{}/bot{token}/{method}", self.telegram_base)
    }

    pub fn gemini_url(&self, model: &str) -> String {
        format!("{}/{model}:generateContent", self.gemini_base)
    }

    pub fn gemini_model_url(&self, model: &str) -> String {
        format!("{}/{model}", self.gemini_base)
    }

    pub fn groq_models_url(&self) -> String {
        format!("{}/models", self.groq_base)
    }

    // Deliberately absent from production: credentials never go to an endpoint
    // selected through an environment variable or a command-line argument.
    #[cfg(test)]
    pub fn fixture(base: &str) -> Self {
        let url = reqwest::Url::parse(base).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        Self {
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            telegram_base: base.into(),
            gemini_base: format!("{base}/models"),
            groq_base: format!("{base}/groq"),
            groq_url: format!("{base}/chat/completions"),
        }
    }
}

pub fn check_models(
    cfg: &crate::config::Config,
    services: &Services,
    budget: &Budget,
) -> Result<()> {
    let gemini_key = cfg.gemini_key()?;
    let groq_key = cfg.groq_key()?;
    let model = provider_request(
        "gemini",
        "model_readiness",
        budget,
        Duration::from_secs(20),
        || {
            services
                .client
                .get(services.gemini_model_url(&cfg.gemini_model))
                .header("x-goog-api-key", gemini_key)
        },
    )?;
    if !model
        .get("supportedGenerationMethods")
        .and_then(|methods| methods.as_array())
        .is_some_and(|methods| {
            methods
                .iter()
                .any(|method| method.as_str() == Some("generateContent"))
        })
    {
        return Err(ProviderError::invalid(
            "gemini",
            "model_readiness",
            "model does not advertise generateContent",
        )
        .into());
    }
    // Model IDs can contain '/', so use the documented list endpoint rather
    // than relying on how the API/router interprets an encoded path segment.
    let response = provider_request(
        "groq",
        "model_readiness",
        budget,
        Duration::from_secs(20),
        || {
            services
                .client
                .get(services.groq_models_url())
                .bearer_auth(groq_key)
        },
    )?;
    let models = response
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or_else(|| ProviderError::invalid("groq", "model_readiness", "malformed model list"))?;
    if !models.iter().any(|model| {
        model.get("id").and_then(|value| value.as_str()) == Some(cfg.groq_model.as_str())
            && model.get("active").and_then(|value| value.as_bool()) == Some(true)
    }) {
        return Err(ProviderError::invalid(
            "groq",
            "model_readiness",
            "configured model is missing or inactive",
        )
        .into());
    }
    Ok(())
}

#[derive(Debug)]
pub enum ProviderErrorKind {
    Http(StatusCode),
    Transport {
        timeout: bool,
        connect: bool,
        body: bool,
    },
    InvalidResponse(&'static str),
}

#[derive(Debug)]
pub struct ProviderError {
    provider: &'static str,
    stage: &'static str,
    kind: ProviderErrorKind,
    retry_not_before: Option<u64>,
}

impl ProviderError {
    pub fn http(provider: &'static str, stage: &'static str, status: StatusCode) -> Self {
        Self {
            provider,
            stage,
            kind: ProviderErrorKind::Http(status),
            retry_not_before: None,
        }
    }

    pub fn invalid(provider: &'static str, stage: &'static str, reason: &'static str) -> Self {
        Self {
            provider,
            stage,
            kind: ProviderErrorKind::InvalidResponse(reason),
            retry_not_before: None,
        }
    }

    pub fn retryable(&self) -> bool {
        match self.kind {
            ProviderErrorKind::Http(status) => retryable_status(status),
            ProviderErrorKind::Transport {
                timeout,
                connect,
                body,
            } => timeout || connect || body,
            ProviderErrorKind::InvalidResponse(_) => false,
        }
    }

    pub fn retry_not_before(&self) -> Option<u64> {
        self.retry_not_before
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider={} stage={} ", self.provider, self.stage)?;
        match self.kind {
            ProviderErrorKind::Http(status) => write!(f, "HTTP {status}"),
            ProviderErrorKind::Transport {
                timeout,
                connect,
                body,
            } => {
                write!(
                    f,
                    "transport failure timeout={timeout} connect={connect} body={body}"
                )
            }
            ProviderErrorKind::InvalidResponse(reason) => write!(f, "invalid response: {reason}"),
        }
    }
}

impl std::error::Error for ProviderError {}

fn retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 429)
}

/// Convert either Retry-After representation to persistent Unix metadata.
/// Round waits up, then retain one extra second so a truncated wall clock never
/// permits the next scheduled invocation to retry before the requested delay.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    let now_seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let seconds = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse::<u64>().ok()?
    } else {
        let deadline = httpdate::parse_http_date(value).ok()?;
        let remaining = deadline.duration_since(now).unwrap_or_default();
        remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() != 0))
    };
    Some(now_seconds.saturating_add(seconds).saturating_add(1))
}

/// Bounded, idempotent provider retries. No request URL, headers, response body,
/// transcript, or generated text is ever included in diagnostics.
pub fn provider_request(
    provider: &'static str,
    stage: &'static str,
    budget: &Budget,
    timeout: Duration,
    mut request: impl FnMut() -> RequestBuilder,
) -> Result<serde_json::Value> {
    for attempt in 1..=MAX_HTTP_ATTEMPTS {
        let started = Instant::now();
        let response = request().timeout(budget.request_timeout(timeout)?).send();
        let error = match response {
            Ok(response) => {
                let status = response.status();
                eprintln!(
                    "provider={provider} stage={stage} attempt={attempt} status={} elapsed_ms={}",
                    status.as_u16(),
                    started.elapsed().as_millis()
                );
                if status.is_success() {
                    // reqwest can label an interrupted body transfer as a
                    // decode error too. Complete the transfer before parsing
                    // JSON so network failures never become terminal syntax
                    // errors, and keep both operations inside this retry bound.
                    match response.bytes() {
                        Ok(body) => {
                            return serde_json::from_slice(&body).map_err(|_| {
                                ProviderError::invalid(provider, stage, "malformed JSON").into()
                            });
                        }
                        Err(error) => {
                            let error = ProviderError {
                                provider,
                                stage,
                                kind: ProviderErrorKind::Transport {
                                    timeout: error.is_timeout(),
                                    connect: error.is_connect(),
                                    body: true,
                                },
                                retry_not_before: None,
                            };
                            eprintln!(
                                "{error} attempt={attempt} elapsed_ms={}",
                                started.elapsed().as_millis()
                            );
                            error
                        }
                    }
                } else {
                    let retry_not_before = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| parse_retry_after(value, SystemTime::now()));
                    let mut error = ProviderError::http(provider, stage, status);
                    error.retry_not_before = retry_not_before;
                    error
                }
            }
            Err(error) => {
                let error = ProviderError {
                    provider,
                    stage,
                    kind: ProviderErrorKind::Transport {
                        timeout: error.is_timeout(),
                        connect: error.is_connect(),
                        body: false,
                    },
                    retry_not_before: None,
                };
                eprintln!(
                    "{error} attempt={attempt} elapsed_ms={}",
                    started.elapsed().as_millis()
                );
                error
            }
        };
        let retry_after = error
            .retry_not_before()
            .map(|deadline| deadline.saturating_sub(unix_time()));
        // A longer server-requested wait belongs to the next scheduled run;
        // never retry earlier than Retry-After just to fit a local backoff cap.
        if !error.retryable()
            || attempt == MAX_HTTP_ATTEMPTS
            || retry_after.is_some_and(|seconds| seconds > 10)
        {
            return Err(error.into());
        }
        budget.delay(Duration::from_secs(
            retry_after.unwrap_or(u64::from(attempt)),
        ))?;
    }
    unreachable!("bounded retry loop always returns")
}

pub fn completion_log(provider: &str, finish: &str, input: Option<u64>, output: Option<u64>) {
    let finish = match finish {
        "STOP" | "MAX_TOKENS" | "SAFETY" | "RECITATION" | "stop" | "length" | "content_filter" => {
            finish
        }
        _ => "other",
    };
    eprintln!(
        "provider={provider} finish_reason={finish} input_tokens={} output_tokens={}",
        input.map_or_else(|| "unknown".into(), |n| n.to_string()),
        output.map_or_else(|| "unknown".into(), |n| n.to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_retry_after_preserves_a_conservative_absolute_deadline() {
        let now = UNIX_EPOCH + Duration::from_millis(1_700_000_000_900);
        assert_eq!(parse_retry_after("3600", now), Some(1_700_003_601));
        assert_eq!(parse_retry_after(" 0 ", now), Some(1_700_000_001));
        assert_eq!(
            parse_retry_after(&u64::MAX.to_string(), now),
            Some(u64::MAX)
        );
        for value in ["", "-1", "+1", "1.5", "not-a-date"] {
            assert_eq!(parse_retry_after(value, now), None, "accepted {value}");
        }
    }

    #[test]
    fn http_date_retry_after_rounds_up_without_underflow_for_past_dates() {
        let now = UNIX_EPOCH + Duration::from_millis(1_445_412_000_900);
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(1_445_412_481)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(1_445_412_001)
        );
    }

    #[test]
    fn synthetic_provider_errors_have_no_server_retry_deadline() {
        assert_eq!(
            ProviderError::http("fixture", "stage", StatusCode::TOO_MANY_REQUESTS)
                .retry_not_before(),
            None
        );
        assert_eq!(
            ProviderError::invalid("fixture", "stage", "malformed JSON").retry_not_before(),
            None
        );
    }
}
