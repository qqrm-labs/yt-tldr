use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

const API: &str = "https://api.telegram.org";
const MAX_MESSAGE_LEN: usize = 3900;

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
    // Без #[serde(default)]: на generic-поле он навешивает T: Default.
    // На 2xx Telegram всегда присылает result (ok:false приходит с 4xx и режется в decode).
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}

fn decode<T: DeserializeOwned>(resp: reqwest::blocking::Response) -> Result<T> {
    let status = resp.status();
    if !status.is_success() {
        bail!("Telegram HTTP {status}");
    }
    Ok(resp.json::<T>()?)
}

fn call<T: DeserializeOwned>(method: &str, token: &str, body: serde_json::Value) -> Result<T> {
    let url = format!("{API}/bot{token}/{method}");
    let resp: TgResponse<T> = decode(
        client()
            .post(url)
            .json(&body)
            .send()
            .map_err(reqwest::Error::without_url)?,
    )?;
    if !resp.ok {
        bail!(
            "telegram {method}: {}",
            resp.description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    resp.result.context("telegram: empty result")
}

pub fn get_updates(cfg: &Config, offset: u64) -> Result<Vec<Update>> {
    call(
        "getUpdates",
        cfg.telegram_token()?,
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

pub fn send_chunk(cfg: &Config, chat_id: i64, text: &str) -> Result<()> {
    let _: serde_json::Value = call(
        "sendMessage",
        cfg.telegram_token()?,
        json!({
            "chat_id": chat_id, "text": text, "disable_web_page_preview": true,
        }),
    )?;
    Ok(())
}

pub fn setup_commands(cfg: &Config) -> Result<()> {
    let _: serde_json::Value = call(
        "setMyCommands",
        cfg.telegram_token()?,
        json!({
            "commands": [
                {"command": "start", "description": "Как пользоваться ботом"},
                {"command": "help", "description": "Как пользоваться ботом"},
            ],
        }),
    )?;
    eprintln!("bot commands are set");
    Ok(())
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
