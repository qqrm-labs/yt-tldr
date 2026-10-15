//! Commit state remotely before permitting the next external side effect.
use anyhow::{Context, Result};
use std::{path::Path, process::Command};

pub fn save(path: &Path) -> Result<()> {
    if let Some(helper) = std::env::var_os("DELIVERY_CHECKPOINT") {
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
    }
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
