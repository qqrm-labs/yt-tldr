use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Config {
    pub telegram_token: Option<String>,
    pub gemini_key: Option<String>,
    pub groq_key: Option<String>,
    pub gemini_model: String,
    pub groq_model: String,
    pub state_path: String,
    allowed_chats: Vec<i64>,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            telegram_token: var("TELEGRAM_BOT_TOKEN"),
            gemini_key: var("GEMINI_API_KEY"),
            groq_key: var("GROQ_API_KEY"),
            gemini_model: var("GEMINI_MODEL").unwrap_or_else(|| "gemini-3.8-flash".to_string()),
            groq_model: var("GROQ_MODEL").unwrap_or_else(|| "openai/gpt-oss-120b".to_string()),
            state_path: var("STATE_PATH").unwrap_or_else(|| "data/bot-state.json".to_string()),
            allowed_chats: parse_allowlist(var("ALLOWED_CHAT_IDS").as_deref())?,
        })
    }

    pub fn telegram_token(&self) -> Result<&str> {
        self.telegram_token
            .as_deref()
            .context("TELEGRAM_BOT_TOKEN не задан")
    }

    pub fn gemini_key(&self) -> Result<&str> {
        self.gemini_key
            .as_deref()
            .context("GEMINI_API_KEY не задан")
    }

    pub fn groq_key(&self) -> Result<&str> {
        self.groq_key.as_deref().context("GROQ_API_KEY не задан")
    }

    pub fn validate_poll(&self) -> Result<()> {
        anyhow::ensure!(
            !self.allowed_chats.is_empty(),
            "ALLOWED_CHAT_IDS is required before polling Telegram"
        );
        Ok(())
    }

    pub fn chat_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chats.contains(&chat_id)
    }
}

fn parse_allowlist(value: Option<&str>) -> Result<Vec<i64>> {
    value
        .map(|value| {
            value
                .split(',')
                .map(|id| {
                    id.trim()
                        .parse::<i64>()
                        .context("ALLOWED_CHAT_IDS must contain comma-separated integer chat IDs")
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist_is_explicit_and_never_silently_drops_invalid_ids() {
        assert!(parse_allowlist(None).unwrap().is_empty());
        assert_eq!(parse_allowlist(Some("42, -100123")).unwrap(), [42, -100123]);
        for invalid in ["42,typo", "42,", "", "*"] {
            assert!(parse_allowlist(Some(invalid)).is_err());
        }
    }
}
