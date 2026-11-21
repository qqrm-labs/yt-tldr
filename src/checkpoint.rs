//! Commit state remotely before permitting the next external side effect.
use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

pub fn save(path: &Path, timeout: Duration) -> Result<()> {
    if let Some(helper) = std::env::var_os("DELIVERY_CHECKPOINT") {
        save_with_helper(path, &helper, timeout)?;
    }
    Ok(())
}

fn save_with_helper(path: &Path, helper: &OsStr, timeout: Duration) -> Result<()> {
    let mut child = Command::new("python3")
        .arg(helper)
        .arg("save")
        .arg(path)
        .spawn()
        .context("cannot start remote checkpoint helper")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .context("cannot wait for remote checkpoint helper")?
        {
            anyhow::ensure!(
                status.success(),
                "remote checkpoint failed; refusing further delivery"
            );
            return Ok(());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("remote checkpoint timed out; refusing further delivery");
        }
        std::thread::sleep(
            Duration::from_millis(20).min(timeout.saturating_sub(started.elapsed())),
        );
    }
}

pub fn require_hosted_configuration() -> Result<()> {
    anyhow::ensure!(
        std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
            || std::env::var_os("DELIVERY_CHECKPOINT").is_some(),
        "hosted execution requires a durable checkpoint helper"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn hosted_python_helper_receives_the_real_state_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state with spaces.json");
        std::fs::write(&path, br#"{"cursor":7}"#).unwrap();
        let helper = directory.path().join("checkpoint helper.py");
        std::fs::write(
            &helper,
            r#"
import json, pathlib, sys
assert sys.argv[1] == 'save'
path = pathlib.Path(sys.argv[2])
assert json.loads(path.read_text())['cursor'] == 7
path.with_suffix('.receipt').write_text('durable')
"#,
        )
        .unwrap();
        save_with_helper(&path, helper.as_os_str(), Duration::from_secs(5)).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("receipt")).unwrap(),
            "durable"
        );
    }

    #[test]
    fn helper_failure_reaches_the_delivery_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("failed.py");
        std::fs::write(&helper, "raise SystemExit(1)\n").unwrap();
        assert!(
            save_with_helper(
                &directory.path().join("state.json"),
                helper.as_os_str(),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }

    #[test]
    fn hung_checkpoint_helper_has_a_finite_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("hung.py");
        std::fs::write(&helper, "import time\ntime.sleep(60)\n").unwrap();
        let started = Instant::now();
        let error = save_with_helper(
            &directory.path().join("state.json"),
            helper.as_os_str(),
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
