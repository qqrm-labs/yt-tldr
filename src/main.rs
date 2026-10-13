mod config;
mod gemini;
mod groq;
mod runner;
mod state;
mod telegram;
mod util;
mod youtube;

use anyhow::{Context, Result};
use config::Config;

const HELP: &str = "🎬 Кидай ссылку на YouTube-видео (обычную, youtu.be или /shorts/) — пришлю саммари с ключевыми тейками.\n\nОбработка занимает пару минут.";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().unwrap_or_default().as_str() {
        "poll" => poll(),
        "setup" => telegram::setup_commands(&Config::from_env()?),
        "process" => {
            let url = args
                .next()
                .context("usage: yt-tldr process <youtube-url>")?;
            let out = process_video(&Config::from_env()?, &url)?;
            println!("{out}");
            Ok(())
        }
        _ => {
            eprintln!("usage: yt-tldr <poll|setup|process <youtube-url>>");
            std::process::exit(2);
        }
    }
}

/// Один прогон cron-джоба: вычерпать очередь getUpdates, обработать, сохранить стейт.
fn poll() -> Result<()> {
    let cfg = Config::from_env()?;
    if cfg.telegram_token.is_none() {
        eprintln!("TELEGRAM_BOT_TOKEN is not set yet - bot is not provisioned, skipping run");
        return Ok(());
    }
    cfg.validate_poll()?;
    let mut state = state::State::load(&cfg.state_path)?;
    let process = |state: &mut state::State, update: &telegram::Update| {
        runner::process_update(
            state,
            update,
            update
                .message
                .as_ref()
                .is_some_and(|message| cfg.chat_allowed(message.chat.id)),
            |text| handle_text(&cfg, text),
            |chat, text| telegram::send_chunk(&cfg, chat, text),
            |state| state.save(&cfg.state_path),
        )
    };
    let mut handled = 0;
    while handled < 25 {
        let updates = telegram::get_updates(&cfg, state.update_offset)?;
        if updates.is_empty() {
            anyhow::ensure!(
                state.pending.is_none(),
                "pending update is unavailable from Telegram; operator reconciliation required"
            );
            break;
        }
        for update in updates.into_iter().take(25 - handled) {
            process(&mut state, &update)?;
            handled += 1;
        }
    }
    Ok(())
}

fn handle_text(cfg: &Config, text: &str) -> Result<Option<String>> {
    if text == "/start" || text == "/help" {
        return Ok(Some(HELP.to_string()));
    }
    if text.starts_with('/') {
        return Ok(Some(
            "Не знаю такую команду. Просто кинь ссылку на YouTube.".to_string(),
        ));
    }
    match youtube::extract_youtube_url(text) {
        Some(url) => Ok(Some(process_video(cfg, &url)?)),
        None => Ok(None),
    }
}

fn process_video(cfg: &Config, video_url: &str) -> Result<String> {
    let Some(t) = gemini::transcribe(cfg, video_url)? else {
        return Ok("Не нашёл в этом видео речи — саммарить нечего 🤷".to_string());
    };
    let summary = groq::summarize(cfg, &t.text)?;
    let mut out = String::new();
    if let Some(title) = &t.title {
        out.push_str(&format!("🎬 {title}\n\n"));
    }
    out.push_str(&summary);
    Ok(out)
}
