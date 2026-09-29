use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Хвост дедуп-списка, который держим в стейте.
const KEEP_LAST: usize = 2000;

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub update_offset: u64,
    #[serde(default)]
    pub processed: Vec<String>,
}

impl State {
    pub fn load(path: &str) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_processed(&self, key: &str) -> bool {
        self.processed.iter().any(|k| k == key)
    }

    pub fn mark_processed(&mut self, key: &str) {
        self.processed.push(key.to_string());
        if self.processed.len() > KEEP_LAST {
            let drop = self.processed.len() - KEEP_LAST;
            self.processed.drain(..drop);
        }
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
