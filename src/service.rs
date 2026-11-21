use crate::{app, config::Config, http::Services, runtime::Budget, state::State};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LONG_POLL_SECONDS: u32 = 25;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(10);
const MIN_CYCLE_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(unix)]
const LOCK_FILE_NAME: &str = ".yt-tldr.lock";

pub fn poll_once(cfg: &Config, services: &Services, budget: &Budget) -> Result<usize> {
    require_host_runtime()?;
    cfg.validate_poll()?;
    let _lock = StateLock::acquire(Path::new(&cfg.state_path))?;
    app::poll(cfg, services, budget)
}

pub fn serve(cfg: &Config, services: &Services) -> Result<()> {
    require_host_runtime()?;
    cfg.validate_poll()?;
    let _lock = StateLock::acquire(Path::new(&cfg.state_path))?;
    let mut state = State::load(&cfg.state_path)?;
    let mut retry = RetryBackoff::new();
    loop {
        let cooldown = telegram_cooldown(&state, crate::runtime::unix_time());
        if !cooldown.is_zero() {
            eprintln!("startup_throttled retry_in_secs={}", cooldown.as_secs());
            wait_for(cooldown);
            continue;
        }
        let budget = Budget::new(crate::runtime::POLL_BUDGET);
        match app::initialize_poll(cfg, services, &budget, &mut state, |state| {
            state.save(&cfg.state_path, budget.checkpoint_timeout()?)
        }) {
            Ok(()) => break,
            Err(error) if app::retryable_poll_error(&error, &state) => {
                wait_after_error(&error, &mut retry, "startup");
            }
            Err(error) => return Err(error.context("serve initialization stopped")),
        }
    }
    eprintln!("serve_started long_poll_seconds={LONG_POLL_SECONDS}");
    retry.reset();
    loop {
        // Each cycle and request remain bounded. Durable state, including provider
        // attempt limits and delivery intents, is retained across these budgets.
        let budget = Budget::new(crate::runtime::POLL_BUDGET);
        let started = Instant::now();
        match app::poll_cycle(
            cfg,
            services,
            &budget,
            &mut state,
            |state| state.save(&cfg.state_path, budget.checkpoint_timeout()?),
            LONG_POLL_SECONDS,
        ) {
            Ok(count) => {
                retry.reset();
                if count > 0 {
                    eprintln!("poll_completed updates={count}");
                }
                let wait = cycle_delay(&state, crate::runtime::unix_time(), started.elapsed());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
            Err(error) if app::retryable_poll_error(&error, &state) => {
                wait_after_error(&error, &mut retry, "poll");
            }
            Err(error) => {
                return Err(error.context("serve stopped; operator review is required"));
            }
        }
    }
}

fn wait_after_error(error: &anyhow::Error, retry: &mut RetryBackoff, phase: &str) {
    let wait = retry_delay(error, retry.next_delay());
    // Never log raw HTTP errors: they can contain request URLs or provider
    // details. A server-provided cooldown takes precedence over normal backoff.
    eprintln!("{phase}_retry_deferred retry_in_ms={}", wait.as_millis());
    wait_for(wait);
}

fn wait_for(wait: Duration) {
    let started = Instant::now();
    loop {
        let remaining = wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(MAX_RETRY_DELAY));
    }
}

fn telegram_cooldown(state: &State, now: u64) -> Duration {
    Duration::from_secs(
        state
            .telegram_retry_not_before
            .unwrap_or(0)
            .saturating_sub(now),
    )
}

fn retry_delay(error: &anyhow::Error, backoff: Duration) -> Duration {
    error
        .downcast_ref::<crate::telegram::TelegramRejection>()
        .filter(|rejection| rejection.error_code == 429)
        .and_then(|rejection| rejection.retry_after)
        .map(|seconds| Duration::from_secs(seconds.saturating_add(1)).max(backoff))
        .unwrap_or(backoff)
}

fn require_host_runtime() -> Result<()> {
    validate_host_runtime(std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"))
}

fn validate_host_runtime(github_actions: bool) -> Result<()> {
    ensure!(
        !github_actions,
        "Telegram polling requires a persistent host; GitHub Actions polling is disabled"
    );
    Ok(())
}

fn cycle_delay(state: &State, now: u64, elapsed: Duration) -> Duration {
    if let Some(until) = state.next_retry_deadline().filter(|until| *until > now) {
        return Duration::from_secs(until - now).min(PENDING_POLL_INTERVAL);
    }
    // Telegram normally holds an idle getUpdates request for 25 seconds. Keep
    // a short floor as well, in case it repeatedly returns an empty batch early.
    MIN_CYCLE_INTERVAL.saturating_sub(elapsed)
}

struct RetryBackoff {
    failures: u32,
    jitter: u64,
}

impl RetryBackoff {
    fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            ^ u64::from(std::process::id());
        Self {
            failures: 0,
            jitter: seed.max(1),
        }
    }

    fn reset(&mut self) {
        self.failures = 0;
    }

    fn next_delay(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.jitter ^= self.jitter << 13;
        self.jitter ^= self.jitter >> 7;
        self.jitter ^= self.jitter << 17;
        let base_millis = (1_u64 << self.failures.min(5)) * 1_000;
        Duration::from_millis(base_millis + self.jitter % 1_000).min(MAX_RETRY_DELAY)
    }
}

// Hold the descriptor for the entire consumer lifetime. Never delete the lock
// file: deleting it would let another process lock a different inode.
struct StateLock {
    _file: File,
}

impl StateLock {
    #[cfg(unix)]
    fn acquire(state_path: &Path) -> Result<Self> {
        use std::{
            fs::{self, DirBuilder, OpenOptions},
            io::ErrorKind,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            path::Component,
        };

        ensure!(
            state_path.is_absolute()
                && state_path.file_name().is_some()
                && state_path
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "STATE_PATH must be an absolute path in a private persistent directory"
        );
        let parent = state_path.parent().context("STATE_PATH has no parent")?;
        ensure!(
            state_path.file_name() != Some(std::ffi::OsStr::new(LOCK_FILE_NAME)),
            "STATE_PATH must not use the reserved consumer lock filename"
        );
        match fs::symlink_metadata(parent) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .context("cannot create private state directory")?;
            }
            Err(error) => return Err(error).context("cannot inspect state directory"),
            Ok(_) => {}
        }
        let metadata = fs::symlink_metadata(parent).context("cannot inspect state directory")?;
        ensure!(
            metadata.file_type().is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "STATE_PATH parent must be a private directory (mode 0700), not a symlink"
        );
        require_private_file(state_path)?;

        // One consumer per state directory, even if a caller changes the state
        // filename. This cannot coordinate independent hosts or directories.
        let lock_path = parent.join(LOCK_FILE_NAME);
        require_private_file(&lock_path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .context("cannot open state lock")?;
        require_private_file(&lock_path)?;
        file.try_lock()
            .context("cannot acquire state lock; another Telegram consumer may be running")?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire(_state_path: &Path) -> Result<Self> {
        anyhow::bail!("persistent polling requires Unix private file permissions")
    }
}

#[cfg(unix)]
fn require_private_file(path: &Path) -> Result<()> {
    use std::{fs, io::ErrorKind, os::unix::fs::PermissionsExt};
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file() && metadata.permissions().mode() & 0o077 == 0,
            "state and lock files must be private regular files (mode 0600), not symlinks"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect private state file"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_polling_is_rejected() {
        assert!(validate_host_runtime(true).is_err());
        validate_host_runtime(false).unwrap();
    }

    #[test]
    fn transient_backoff_is_bounded_and_resets_after_success() {
        let mut retry = RetryBackoff {
            failures: 0,
            jitter: 1,
        };
        for _ in 0..100 {
            let delay = retry.next_delay();
            assert!((Duration::from_secs(2)..=MAX_RETRY_DELAY).contains(&delay));
        }
        retry.reset();
        assert!(retry.next_delay() < Duration::from_secs(3));
    }

    #[test]
    fn telegram_cooldown_is_respected_even_without_a_pending_update() {
        let error = crate::telegram::TelegramRejection {
            error_code: 429,
            retry_after: Some(120),
        }
        .into();
        assert_eq!(
            retry_delay(&error, Duration::from_secs(2)),
            Duration::from_secs(121)
        );
        let error = crate::telegram::TelegramRejection {
            error_code: 429,
            retry_after: Some(0),
        }
        .into();
        assert_eq!(
            retry_delay(&error, Duration::from_secs(4)),
            Duration::from_secs(4)
        );
        assert_eq!(
            retry_delay(&anyhow::anyhow!("transport fixture"), MAX_RETRY_DELAY),
            MAX_RETRY_DELAY
        );
        let state = State {
            telegram_retry_not_before: Some(221),
            ..State::default()
        };
        assert_eq!(telegram_cooldown(&state, 100), Duration::from_secs(121));
        assert_eq!(telegram_cooldown(&state, 221), Duration::ZERO);
        assert_eq!(
            cycle_delay(&state, 100, Duration::ZERO),
            PENDING_POLL_INTERVAL
        );
    }

    #[test]
    fn idle_cycles_cannot_busy_loop() {
        let state = State::default();
        assert_eq!(cycle_delay(&state, 100, Duration::ZERO), MIN_CYCLE_INTERVAL);
        assert_eq!(
            cycle_delay(&state, 100, Duration::from_secs(25)),
            Duration::ZERO
        );
    }

    #[test]
    fn delayed_updates_keep_polling_boundaries_without_retrying_early() {
        let state: State = serde_json::from_value(serde_json::json!({
            "pending": {
                "update_id": 1,
                "attempts": 1,
                "reply_hash": null,
                "next_chunk": 0,
                "retry_not_before": 200
            }
        }))
        .unwrap();
        assert_eq!(
            cycle_delay(&state, 100, Duration::ZERO),
            PENDING_POLL_INTERVAL
        );
        assert_eq!(
            cycle_delay(&state, 199, Duration::ZERO),
            Duration::from_secs(1)
        );
        assert_eq!(
            cycle_delay(&state, 200, Duration::from_secs(25)),
            Duration::ZERO
        );
        assert_eq!(state.pending.unwrap().retry_not_before, Some(200));
    }

    #[cfg(unix)]
    #[test]
    fn private_state_lock_is_exclusive_and_released_on_drop() {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};
        let temp = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = temp.path().join("state.json");
        assert!(StateLock::acquire(&temp.path().join(LOCK_FILE_NAME)).is_err());
        let lock = StateLock::acquire(&path).unwrap();
        assert!(StateLock::acquire(&path).is_err());
        assert!(StateLock::acquire(&temp.path().join("other.json")).is_err());
        drop(lock);
        StateLock::acquire(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_requires_absolute_private_directory_and_private_regular_file() {
        use std::{fs, os::unix::fs::PermissionsExt};
        assert!(StateLock::acquire(Path::new("data/state.json")).is_err());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(StateLock::acquire(&path).is_err());
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(StateLock::acquire(&path).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path().join("elsewhere.json"), &path).unwrap();
        assert!(StateLock::acquire(&path).is_err());
    }
}
