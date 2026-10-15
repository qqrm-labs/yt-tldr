//! Commit state remotely before permitting the next external side effect.
use anyhow::{Context, Result};
use std::{ffi::OsStr, path::Path, process::Command};

pub fn save(path: &Path) -> Result<()> {
    if let Some(helper) = std::env::var_os("DELIVERY_CHECKPOINT") {
        save_with_helper(path, &helper)?;
    }
    Ok(())
}

fn save_with_helper(path: &Path, helper: &OsStr) -> Result<()> {
    anyhow::ensure!(
        Command::new("python3")
            .arg(helper)
            .arg("save")
            .arg(path)
            .status()
            .context("cannot start remote checkpoint helper")?
            .success(),
        "remote checkpoint failed; refusing further delivery"
    );
    Ok(())
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
        save_with_helper(&path, helper.as_os_str()).unwrap();
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
            save_with_helper(&directory.path().join("state.json"), helper.as_os_str()).is_err()
        );
    }
}
