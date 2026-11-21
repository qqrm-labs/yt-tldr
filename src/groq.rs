use crate::{
    config::Config,
    http::{self, ProviderError, Services},
    runtime::Budget,
    util::truncate,
};
use anyhow::Result;
use serde_json::json;
use std::time::Duration;

const SYSTEM: &str = "Ты — редактор, который делает выжимки из расшифровок видео. Тебе дают транскрипт, он может содержать ошибки автораспознавания — восстанавливай смысл по контексту. Пиши саммари НА ЯЗЫКЕ ТРАНСКРИПТА. Формат без markdown (текст уходит в Telegram как plain text): первая строка 'TL;DR:' и 1-3 предложения о сути видео. Затем тейки: 3-7 пунктов, каждый с новой строки, начинается с '• ', не длиннее 25 слов, с конкретикой — факты, цифры, имена, выводы. Без воды и без фраз вроде 'в этом видео мы рассмотрим'.";
const MAX_TRANSCRIPT_CHARS: usize = 120_000;

pub struct Summary {
    pub text: String,
    pub truncated: bool,
}

pub fn summarize(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    transcript: &str,
) -> Result<Summary> {
    let key = cfg.groq_key()?;
    let input_truncated = transcript.chars().count() > MAX_TRANSCRIPT_CHARS;
    let body = json!({
        "model": cfg.groq_model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": truncate(transcript, MAX_TRANSCRIPT_CHARS)},
        ],
        "temperature": 0.3,
        "max_tokens": 2048,
    });
    let value =
        http::provider_request("groq", "summarize", budget, Duration::from_secs(60), || {
            services
                .client
                .post(&services.groq_url)
                .bearer_auth(key)
                .json(&body)
        })?;
    let finish = value
        .pointer("/choices/0/finish_reason")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    http::completion_log(
        "groq",
        finish,
        value
            .pointer("/usage/prompt_tokens")
            .and_then(|value| value.as_u64()),
        value
            .pointer("/usage/completion_tokens")
            .and_then(|value| value.as_u64()),
    );
    let output_truncated = match finish {
        "stop" => false,
        "length" => true,
        _ => {
            return Err(ProviderError::invalid(
                "groq",
                "summarize",
                "missing, blocked, or unsupported finish reason",
            )
            .into());
        }
    };
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ProviderError::invalid("groq", "summarize", "empty summary"))?;
    Ok(Summary {
        text: text.into(),
        truncated: input_truncated || output_truncated,
    })
}
