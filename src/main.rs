mod app;
mod checkpoint;
mod config;
mod gemini;
mod groq;
mod http;
mod progress;
mod runner;
mod runtime;
mod service;
mod state;
mod telegram;
mod util;
mod youtube;

#[cfg(test)]
mod integration_tests;

use anyhow::{Context, Result};
use config::Config;
use http::Services;
use runtime::Budget;
use std::time::Duration;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().unwrap_or_default().as_str() {
        "poll" => {
            let count = service::poll_once(
                &Config::from_env()?,
                &Services::new()?,
                &Budget::new(runtime::POLL_BUDGET),
            )?;
            eprintln!("poll_completed updates={count}");
            Ok(())
        }
        "serve" => {
            let result = (|| service::serve(&Config::from_env()?, &Services::new()?))();
            if let Err(error) = result {
                eprintln!("serve_stopped: {error:#}");
                // Configuration, checkpoint, and ambiguous-delivery failures need
                // an operator. Supervisors must not restart this exit status.
                std::process::exit(78);
            }
            Ok(())
        }
        "doctor" => {
            let report = app::doctor(
                &Config::from_env()?,
                &Services::new()?,
                &Budget::new(Duration::from_secs(5 * 60)),
            )?;
            println!("{report}");
            Ok(())
        }
        "setup" => telegram::setup_commands(
            &Config::from_env()?,
            &Services::new()?,
            &Budget::new(Duration::from_secs(3 * 60)),
        ),
        "process" => {
            let url = args
                .next()
                .context("usage: yt-tldr process <youtube-url>")?;
            let out = app::process_video(
                &Config::from_env()?,
                &Services::new()?,
                &Budget::new(Duration::from_secs(15 * 60)),
                &url,
            )?;
            println!("{out}");
            Ok(())
        }
        _ => {
            eprintln!("usage: yt-tldr <serve|poll|doctor|setup|process <youtube-url>>");
            std::process::exit(2);
        }
    }
}
