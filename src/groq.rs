use crate::config::Config;
use crate::util::truncate;
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::time::Duration;

const SYSTEM: &str = "Ты — редактор, который делает выжимки из расшифровок видео. Тебе дают транскрипт, он может содержать ошибки автораспознавания — восстанавливай смысл по контексту. Пиши саммари НА ЯЗЫКЕ ТРАНСКРИПТА. Формат без markdown (текст уходит в Telegram как plain text): первая строка 'TL;DR:' и 1-3 предложения о сути видео. Затем тейки: 3-7 пунктов, каждый с новой строки, начинается с '• ', не длиннее 25 слов, с конкретикой — факты, цифры, имена, выводы. Без воды и без фраз вроде 'в этом видео мы рассмотрим'.";

pub fn summarize(cfg: &Config, transcript: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let body = json!({
        "model": cfg.groq_model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": truncate(transcript, 120_000)},
        ],
        "temperature": 0.3,
        "max_tokens": 2048,
    });
    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(cfg.groq_key()?)
        .json(&body)
        .send()?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!("Groq {status}: {}", truncate(&text, 400));
    }
    let v: serde_json::Value = resp.json()?;
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .context("Groq не вернул саммари")?;
    Ok(content.to_string())
}
