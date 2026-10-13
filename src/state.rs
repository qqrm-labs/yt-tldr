use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct Pending {
    pub update: crate::telegram::Update,
    pub attempts: u8,
    pub chunks: Option<Vec<String>>,
    pub next_chunk: usize,
}

/// Хвост дедуп-списка, который держим в стейте.
const KEEP_LAST: usize = 2000;

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub update_offset: u64,
    #[serde(default)]
    pub processed: Vec<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
}

impl State {
    pub fn load(path: &str) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let state: Self = serde_json::from_str(&raw)?;
                if let Some(pending) = &state.pending {
                    anyhow::ensure!(
                        pending.next_chunk <= pending.chunks.as_ref().map_or(0, Vec::len),
                        "invalid delivery cursor in stored state"
                    );
                    anyhow::ensure!(
                        pending.update.update_id >= state.update_offset,
                        "pending update precedes stored offset"
                    );
                }
                Ok(state)
            }
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
        let parent = Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(self)?)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_state_loads_and_atomic_checkpoints_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let path = path.to_str().unwrap();
        assert_eq!(State::load(path).unwrap().update_offset, 0);
        std::fs::write(path, r#"{"update_offset":42,"processed":["1:2"]}"#).unwrap();
        let mut state = State::load(path).unwrap();
        assert!(state.pending.is_none());
        state.update_offset = 43;
        state.save(path).unwrap();
        assert_eq!(State::load(path).unwrap().update_offset, 43);
    }
    #[test]
    fn read_failure_or_corrupt_state_never_resets_offset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(State::load(dir.path().to_str().unwrap()).is_err());
        let path = dir.path().join("state.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(State::load(path.to_str().unwrap()).is_err());
    }
}
