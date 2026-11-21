use crate::{
    config::Config,
    http::{self, ProviderError, Services},
    runtime::Budget,
};
use anyhow::Result;
use serde_json::json;
use std::time::Duration;

const PROMPT: &str = "Первая строка ответа — строго в формате: TITLE: <название видео>. Затем выведи ПОЛНЫЙ дословный транскрипт всей речи в видео, без таймкодов и без своих комментариев. Если речи в видео нет вообще — выведи единственную строку: NO_SPEECH";

pub struct Transcript {
    pub title: Option<String>,
    pub text: String,
    pub truncated: bool,
}

pub fn transcribe(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    video_url: &str,
) -> Result<Option<Transcript>> {
    let key = cfg.gemini_key()?;
    let mut generation_config = json!({"maxOutputTokens": 16384});
    // Gemini 3 uses thinkingLevel; thinkingBudget: 0 is not its contract.
    // Other configured models retain their own default thinking behavior.
    if cfg.gemini_model.starts_with("gemini-3") {
        generation_config["thinkingConfig"] = json!({"thinkingLevel": "LOW"});
    }
    let body = json!({
        "contents": [{"parts": [
            {"file_data": {"file_uri": video_url}},
            {"text": PROMPT},
        ]}],
        "generationConfig": generation_config,
    });
    let value = http::provider_request(
        "gemini",
        "transcribe",
        budget,
        Duration::from_secs(120),
        || {
            services
                .client
                .post(services.gemini_url(&cfg.gemini_model))
                .header("x-goog-api-key", key)
                .json(&body)
        },
    )?;
    parse_transcript(value)
}

fn parse_transcript(value: serde_json::Value) -> Result<Option<Transcript>> {
    let invalid = |reason| ProviderError::invalid("gemini", "transcribe", reason);
    let finish = value
        .pointer("/candidates/0/finishReason")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    http::completion_log(
        "gemini",
        finish,
        value
            .pointer("/usageMetadata/promptTokenCount")
            .and_then(|value| value.as_u64()),
        value
            .pointer("/usageMetadata/candidatesTokenCount")
            .and_then(|value| value.as_u64()),
    );
    if value.pointer("/promptFeedback/blockReason").is_some() {
        return Err(invalid("prompt blocked").into());
    }
    let truncated = match finish {
        "STOP" => false,
        "MAX_TOKENS" => true,
        _ => return Err(invalid("missing, blocked, or unsupported finish reason").into()),
    };
    let text = value
        .pointer("/candidates/0/content/parts")
        .and_then(|parts| parts.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("thought").and_then(|value| value.as_bool()) != Some(true))
                .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let text = text.trim();
    if text == "NO_SPEECH" && !truncated {
        return Ok(None);
    }
    let (title, text) = if let Some(rest) = text.strip_prefix("TITLE:") {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        let title = (!line.trim().is_empty()).then(|| line.trim().to_string());
        (title, tail.trim().to_string())
    } else {
        (None, text.to_string())
    };
    if text.is_empty() {
        return Err(invalid("empty transcript").into());
    }
    Ok(Some(Transcript {
        title,
        text,
        truncated,
    }))
}
