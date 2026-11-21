use crate::{
    config::Config,
    http::Services,
    runtime::{Budget, BudgetExhausted},
};
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

const MAX_MESSAGE_LEN: usize = 3900;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Deserialize, Serialize)]
pub struct Update {
    pub update_id: u64,
    pub message: Option<Message>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub text: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Deserialize)]
struct TgResponse<T> {
    ok: bool,
    // Do not add serde(default): it would unnecessarily require T: Default.
    result: Option<T>,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

/// The API explicitly confirmed that this request was rejected.
#[derive(Debug)]
pub struct TelegramRejection {
    pub error_code: u16,
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for TelegramRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Telegram request rejected: API {}", self.error_code)?;
        if let Some(seconds) = self.retry_after {
            write!(f, "; retry_after={seconds}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TelegramRejection {}

/// The execution budget expired before any send request was attempted.
#[derive(Debug)]
pub struct SendNotAttempted;

impl std::fmt::Display for SendNotAttempted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Telegram send was not attempted: execution budget exhausted")
    }
}

impl std::error::Error for SendNotAttempted {}

#[derive(Debug, Deserialize)]
pub struct SendReceipt {
    pub message_id: i64,
}

pub struct DoctorReport {
    pub username: String,
}

fn decode<T: DeserializeOwned>(status: StatusCode, body: &[u8]) -> Result<T> {
    // Neither the response body nor description belongs in logs: Telegram can
    // include supplied text in errors. Decode errors are deliberately static.
    let response: TgResponse<T> = serde_json::from_slice(body)
        .map_err(|_| anyhow::anyhow!("Telegram HTTP {} returned invalid JSON", status.as_u16()))?;
    if !response.ok {
        if let Some(code) = response.error_code
            && code == status.as_u16()
            && matches!(code, 400 | 401 | 403 | 404 | 409 | 429)
        {
            return Err(TelegramRejection {
                error_code: code,
                retry_after: response
                    .parameters
                    .and_then(|parameters| parameters.retry_after),
            }
            .into());
        }
        bail!(
            "Telegram HTTP {} returned an unconfirmed API failure",
            status.as_u16()
        );
    }
    anyhow::ensure!(
        status.is_success(),
        "Telegram HTTP {} returned an inconsistent API result",
        status.as_u16()
    );
    response.result.context("Telegram response omitted result")
}

fn call<T: DeserializeOwned>(
    method: &str,
    token: &str,
    services: &Services,
    budget: &Budget,
    body: serde_json::Value,
) -> Result<T> {
    let timeout = budget.request_timeout(REQUEST_TIMEOUT)?;
    let response = services
        .client
        .post(services.telegram_url(token, method))
        .timeout(timeout)
        .json(&body)
        .send()
        .map_err(|error| {
            // Drop the complete reqwest error, including its URL and sources.
            anyhow::anyhow!(
                "Telegram transport failure: timeout={} connect={}",
                error.is_timeout(),
                error.is_connect()
            )
        })?;
    let status = response.status();
    let body = response
        .bytes()
        .map_err(|_| anyhow::anyhow!("Telegram response body was not received completely"))?;
    decode(status, &body)
}

pub fn get_updates(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    offset: u64,
) -> Result<Vec<Update>> {
    call(
        "getUpdates",
        cfg.telegram_token()?,
        services,
        budget,
        json!({
            "offset": offset,
            "timeout": 0,
            "allowed_updates": ["message"],
        }),
    )
}

pub fn message_chunks(text: &str) -> Vec<String> {
    chunk_text(text, MAX_MESSAGE_LEN)
}

pub fn send_chunk(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    chat_id: i64,
    text: &str,
) -> Result<SendReceipt> {
    let receipt: SendReceipt = call(
        "sendMessage",
        cfg.telegram_token()?,
        services,
        budget,
        json!({
            "chat_id": chat_id, "text": text, "disable_web_page_preview": true,
        }),
    )
    .map_err(|error| {
        if error.downcast_ref::<BudgetExhausted>().is_some() {
            SendNotAttempted.into()
        } else {
            error
        }
    })?;
    anyhow::ensure!(
        receipt.message_id > 0,
        "Telegram returned an invalid message receipt"
    );
    Ok(receipt)
}

pub fn setup_commands(cfg: &Config, services: &Services, budget: &Budget) -> Result<()> {
    let configured: bool = call(
        "setMyCommands",
        cfg.telegram_token()?,
        services,
        budget,
        json!({
            "commands": [
                {"command": "start", "description": "Как пользоваться ботом"},
                {"command": "help", "description": "Как пользоваться ботом"},
            ],
        }),
    )?;
    anyhow::ensure!(configured, "Telegram did not confirm command setup");
    eprintln!("bot commands are set");
    Ok(())
}

pub fn doctor(cfg: &Config, services: &Services, budget: &Budget) -> Result<DoctorReport> {
    #[derive(Deserialize)]
    struct Identity {
        is_bot: bool,
        username: Option<String>,
    }
    #[derive(Deserialize)]
    struct WebhookInfo {
        url: String,
    }
    let identity: Identity = call("getMe", cfg.telegram_token()?, services, budget, json!({}))?;
    anyhow::ensure!(
        identity.is_bot,
        "Telegram credentials do not identify a bot"
    );
    let username = identity.username.context("Telegram bot has no username")?;
    anyhow::ensure!(
        (5..=32).contains(&username.len())
            && username
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "Telegram bot returned an invalid username"
    );
    let webhook: WebhookInfo = call(
        "getWebhookInfo",
        cfg.telegram_token()?,
        services,
        budget,
        json!({}),
    )?;
    anyhow::ensure!(
        webhook.url.is_empty(),
        "Telegram webhook is active; polling cannot start"
    );
    Ok(DoctorReport { username })
}

/// Режем по строкам (падения на границе абзаца приятнее глазу), длинные строки — принудительно.
fn chunk_text(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for line in s.split_inclusive('\n') {
        let line_len = line.chars().count();
        if cur_len > 0 && cur_len + line_len > max {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if line_len > max {
            let mut rest = line;
            while rest.chars().count() > max {
                let head: String = rest.chars().take(max).collect();
                rest = &rest[head.len()..];
                chunks.push(head);
            }
            cur_len = rest.chars().count();
            cur.push_str(rest);
            continue;
        }
        cur_len += line_len;
        cur.push_str(line);
    }
    if cur_len > 0 {
        chunks.push(cur);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_rejections_preserve_code_and_retry_after_without_response_text() {
        for code in [400, 401, 403, 404, 409, 429] {
            let body = serde_json::to_vec(&json!({
                "ok": false,
                "error_code": code,
                "description": "private-response-fixture",
                "parameters": {"retry_after": 23},
            }))
            .unwrap();
            let error = decode::<serde_json::Value>(StatusCode::from_u16(code).unwrap(), &body)
                .unwrap_err();
            let rejection = error.downcast_ref::<TelegramRejection>().unwrap();
            assert_eq!(rejection.error_code, code);
            assert_eq!(rejection.retry_after, Some(23));
            assert!(!format!("{error:#}").contains("private-response-fixture"));
        }
    }

    #[test]
    fn malformed_inconsistent_and_server_responses_never_confirm_rejection() {
        for (status, body) in [
            (400, r#"private-response-fixture"#),
            (
                400,
                r#"{"ok":false,"description":"private-response-fixture"}"#,
            ),
            (
                400,
                r#"{"ok":false,"error_code":403,"description":"private-response-fixture"}"#,
            ),
            (400, r#"{"ok":true,"result":{}}"#),
            (408, r#"{"ok":false,"error_code":408}"#),
            (500, r#"{"ok":false,"error_code":500}"#),
            (502, r#"{"ok":false,"error_code":400}"#),
            (200, r#"{"ok":false,"error_code":429}"#),
            (200, r#"{"ok":true}"#),
        ] {
            let error =
                decode::<serde_json::Value>(StatusCode::from_u16(status).unwrap(), body.as_bytes())
                    .unwrap_err();
            assert!(
                error.downcast_ref::<TelegramRejection>().is_none(),
                "HTTP {status} falsely confirmed a rejection"
            );
            assert!(!format!("{error:#}").contains("private-response-fixture"));
        }
    }

    #[test]
    fn successful_send_decodes_message_receipt_and_missing_receipt_is_ambiguous() {
        let receipt: SendReceipt = decode(
            StatusCode::OK,
            br#"{"ok":true,"result":{"message_id":123}}"#,
        )
        .unwrap();
        assert_eq!(receipt.message_id, 123);
        let error =
            decode::<SendReceipt>(StatusCode::OK, br#"{"ok":true,"result":{}}"#).unwrap_err();
        assert!(error.downcast_ref::<TelegramRejection>().is_none());
    }

    #[test]
    fn exhausted_budget_is_reported_before_a_send_can_be_attempted() {
        let cfg = Config::fixture("unused.json".into());
        let services = Services::fixture("http://127.0.0.1:9");
        let error =
            send_chunk(&cfg, &services, &Budget::new(Duration::ZERO), 42, "fixture").unwrap_err();
        assert!(error.downcast_ref::<SendNotAttempted>().is_some());
    }

    #[test]
    fn short_message_is_one_chunk() {
        assert_eq!(chunk_text("привет", 100), vec!["привет"]);
    }

    #[test]
    fn splits_on_line_boundaries() {
        let got = chunk_text("a\nbb\nccc\n", 4);
        assert_eq!(got, vec!["a\n", "bb\n", "ccc\n"]);
    }

    #[test]
    fn hard_splits_oversized_line() {
        let got = chunk_text("xxxxx\ny", 3);
        assert_eq!(got, vec!["xxx", "xx\n", "y"]);
    }

    #[test]
    fn utf8_boundaries_are_safe() {
        let got = chunk_text("жжжжж", 2);
        assert_eq!(got, vec!["жж", "жж", "ж"]);
    }
}
