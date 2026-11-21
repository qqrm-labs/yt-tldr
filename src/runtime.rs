use anyhow::{Result, bail};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const POLL_BUDGET: Duration = Duration::from_secs(24 * 60);
pub const UPDATE_START_RESERVE: Duration = Duration::from_secs(10 * 60);
const CHECKPOINT_RESERVE: Duration = Duration::from_secs(90);

#[derive(Debug)]
pub struct BudgetExhausted;

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("execution budget exhausted; update retained for the next run")
    }
}

impl std::error::Error for BudgetExhausted {}

pub struct Budget {
    deadline: Instant,
}

impl Budget {
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
        }
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn can_start_update(&self) -> bool {
        self.remaining() >= UPDATE_START_RESERVE
    }

    /// Leave time to record the result even when a network request times out.
    pub fn request_timeout(&self, cap: Duration) -> Result<Duration> {
        self.remaining()
            .checked_sub(CHECKPOINT_RESERVE)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(cap))
            .ok_or_else(|| BudgetExhausted.into())
    }

    pub fn checkpoint_timeout(&self) -> Result<Duration> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            bail!(BudgetExhausted);
        }
        Ok(remaining.min(Duration::from_secs(75)))
    }

    pub fn delay(&self, delay: Duration) -> Result<()> {
        if self.request_timeout(delay)? < delay {
            bail!(BudgetExhausted);
        }
        std::thread::sleep(delay);
        Ok(())
    }
}

pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
