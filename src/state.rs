use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub struct ReconciliationRequired(pub &'static str);

impl std::fmt::Display for ReconciliationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; operator reconciliation required", self.0)
    }
}

impl std::error::Error for ReconciliationRequired {}

#[derive(Debug)]
pub struct CheckpointFailure;
impl std::fmt::Display for CheckpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("durable checkpoint failed; refusing further delivery")
    }
}
impl std::error::Error for CheckpointFailure {}

#[derive(Serialize, Deserialize)]
pub struct Pending {
    pub update_id: u64,
    pub attempts: u8,
    // Replies are process-local only: the state branch is public.
    #[serde(skip)]
    pub chunks: Option<Vec<String>>,
    pub reply_hash: Option<String>,
    pub next_chunk: usize,
    /// Durable intent without a confirmed Telegram acknowledgement.
    #[serde(default)]
    pub awaiting_ack: bool,
    /// A confirmed throttle never becomes an ambiguous delivery intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_not_before: Option<u64>,
    #[serde(default)]
    pub progress: crate::progress::Progress,
    #[serde(default)]
    pub result_failed: bool,
    /// All result receipts are durable; restart must not regenerate or resend.
    #[serde(default)]
    pub delivery_complete: bool,
}

/// Хвост дедуп-списка, который держим в стейте.
const KEEP_LAST: usize = 2000;

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub bot_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_retry_not_before: Option<u64>,
    #[serde(default)]
    pub update_offset: u64,
    #[serde(default)]
    pub processed: Vec<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
}

impl State {
    pub fn next_retry_deadline(&self) -> Option<u64> {
        self.telegram_retry_not_before.max(
            self.pending
                .as_ref()
                .and_then(|pending| pending.retry_not_before),
        )
    }

    pub fn record_telegram_throttle(&mut self, seconds: u64) -> u64 {
        let deadline = crate::runtime::unix_time()
            .saturating_add(seconds)
            .saturating_add(1);
        let deadline = self.telegram_retry_not_before.unwrap_or(0).max(deadline);
        self.telegram_retry_not_before = Some(deadline);
        deadline
    }

    pub fn bind_bot(&mut self, bot_id: i64) -> Result<bool> {
        anyhow::ensure!(bot_id > 0, "invalid Telegram bot identity");
        let identity = fingerprint(format!("telegram-bot:{bot_id}").as_bytes());
        if let Some(bound) = &self.bot_fingerprint {
            anyhow::ensure!(
                bound == &identity,
                ReconciliationRequired("state belongs to another Telegram bot")
            );
            return Ok(false);
        }
        anyhow::ensure!(
            self.update_offset == 0
                && self.processed.is_empty()
                && self.pending.is_none()
                && self.telegram_retry_not_before.is_none(),
            ReconciliationRequired("existing nonempty state has no bot identity binding")
        );
        self.bot_fingerprint = Some(identity);
        Ok(true)
    }

    pub fn load(path: &str) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let mut state: Self = serde_json::from_str(&raw)?;
                for key in &mut state.processed {
                    if !key.starts_with("sha256:") {
                        *key = fingerprint(key.as_bytes());
                    }
                }
                if let Some(pending) = &state.pending {
                    anyhow::ensure!(
                        pending.next_chunk == 0 || pending.reply_hash.is_some(),
                        "invalid delivery cursor in stored state"
                    );
                    anyhow::ensure!(
                        pending.update_id >= state.update_offset,
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
        let hash = fingerprint(key.as_bytes());
        self.processed.iter().any(|k| k == &hash)
    }

    pub fn mark_processed(&mut self, key: &str) {
        self.processed.push(fingerprint(key.as_bytes()));
        if self.processed.len() > KEEP_LAST {
            let drop = self.processed.len() - KEEP_LAST;
            self.processed.drain(..drop);
        }
    }

    pub fn unmark_processed(&mut self, key: &str) {
        let hash = fingerprint(key.as_bytes());
        self.processed.retain(|processed| processed != &hash);
    }

    pub fn save(&self, path: &str, timeout: Duration) -> Result<()> {
        self.write_local(path)?;
        crate::checkpoint::save(Path::new(path), timeout)
    }

    pub fn write_local(&self, path: &str) -> Result<()> {
        let parent = Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(self)?)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

pub fn fingerprint(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_format_remains_compatible_across_sha2_versions() {
        assert_eq!(
            fingerprint(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn legacy_state_loads_and_atomic_checkpoints_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let path = path.to_str().unwrap();
        assert_eq!(State::load(path).unwrap().update_offset, 0);
        std::fs::write(path, r#"{"update_offset":42,"processed":["1:2"]}"#).unwrap();
        let mut state = State::load(path).unwrap();
        assert!(state.pending.is_none());
        assert!(state.is_processed("1:2"));
        state.update_offset = 43;
        state.write_local(path).unwrap();
        assert!(!std::fs::read_to_string(path).unwrap().contains("1:2"));
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
