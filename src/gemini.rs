use crate::config::Config;
use crate::util::truncate;
use anyhow::{Result, bail};
use serde_json::json;
use std::time::Duration;

const PROMPT: &str = "Первая строка ответа — строго в формате: TITLE: <название видео>. Затем выведи ПОЛНЫЙ дословный транскрипт всей речи в видео, без таймкодов и без своих комментариев. Если речи в видео нет вообще — выведи единственную строку: NO_SPEECH";

pub struct Transcript {
    pub title: Option<String>,
    pub text: String,
}

/// Gemini из бесплатного тира понимает YouTube-ссылки нативно: видео не нужно скачивать
/// (yt-dlp с раннеров Actions YouTube часто блокирует). None = речи в видео нет.
pub fn transcribe(cfg: &Config, video_url: &str) -> Result<Option<Transcript>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let body = json!({
        "contents": [{"parts": [
            {"file_data": {"file_uri": video_url}},
            {"text": PROMPT},
        ]}],
        "generationConfig": {
            "thinkingConfig": {"thinkingBudget": 0},
            "maxOutputTokens": 16384,
        },
    });
    let resp = client
        .post(format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            cfg.gemini_model
        ))
        .header("x-goog-api-key", cfg.gemini_key()?)
        .json(&body)
        .send()?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!("Gemini {status}: {}", truncate(&text, 400));
    }
    let v: serde_json::Value = resp.json()?;
    if let Some(reason) = v.pointer("/promptFeedback/blockReason") {
        bail!("Gemini заблокировал запрос: {reason}");
    }
    let text = v
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        bail!(
            "Gemini вернул пустой ответ: {}",
            truncate(&v.to_string(), 400)
        );
    }
    let mut text = text;
    if v.pointer("/candidates/0/finishReason")
        .and_then(|f| f.as_str())
        == Some("MAX_TOKENS")
    {
        text.push_str("\n…[транскрипт обрезан по лимиту]");
    }
    if text.trim().starts_with("NO_SPEECH") {
        return Ok(None);
    }
    let mut title = None;
    if let Some(rest) = text.strip_prefix("TITLE:") {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        title = Some(line.trim().to_string());
        text = tail.trim_start().to_string();
    }
    if text.trim().is_empty() {
        bail!("Gemini не дал транскрипт");
    }
    Ok(Some(Transcript { title, text }))
}
