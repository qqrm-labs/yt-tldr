use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Config {
    pub telegram_token: Option<String>,
    pub gemini_key: Option<String>,
    pub groq_key: Option<String>,
    pub gemini_model: String,
    pub groq_model: String,
    pub state_path: String,
    allowed_chats: Option<Vec<i64>>,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            telegram_token: var("TELEGRAM_BOT_TOKEN"),
            gemini_key: var("GEMINI_API_KEY"),
            groq_key: var("GROQ_API_KEY"),
            gemini_model: var("GEMINI_MODEL").unwrap_or_else(|| "gemini-3.8-flash".to_string()),
            groq_model: var("GROQ_MODEL").unwrap_or_else(|| "openai/gpt-oss-120b".to_string()),
            state_path: var("STATE_PATH").unwrap_or_else(|| "data/bot-state.json".to_string()),
            allowed_chats: var("ALLOWED_CHAT_IDS").map(|s| {
                s.split(',')
                    .filter_map(|p| p.trim().parse::<i64>().ok())
                    .collect()
            }),
        }
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

    /// None = бот открыт всем; список id чатов — белый список.
    pub fn chat_allowed(&self, chat_id: i64) -> bool {
        match &self.allowed_chats {
            None => true,
            Some(ids) => ids.contains(&chat_id),
        }
    }
}
