mod config;
mod gemini;
mod groq;
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
        "setup" => telegram::setup_commands(&Config::from_env()),
        "process" => {
            let url = args
                .next()
                .context("usage: yt-tldr process <youtube-url>")?;
            let out = process_video(&Config::from_env(), &url)?;
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
    let cfg = Config::from_env();
    if cfg.telegram_token.is_none() {
        eprintln!("TELEGRAM_BOT_TOKEN is not set yet - bot is not provisioned, skipping run");
        return Ok(());
    }
    let mut st = state::State::load(&cfg.state_path)?;
    let mut handled = 0u32;
    loop {
        let updates = telegram::get_updates(&cfg, st.update_offset)?;
        if updates.is_empty() {
            break;
        }
        for u in updates {
            st.update_offset = st.update_offset.max(u.update_id + 1);
            let Some(msg) = u.message else { continue };
            let key = format!("{}:{}", msg.chat.id, msg.message_id);
            if st.is_processed(&key) {
                continue;
            }
            // Помечаем до обработки: отравленное сообщение не должно жечь квоты Gemini каждый прогон.
            st.mark_processed(&key);
            if !cfg.chat_allowed(msg.chat.id) {
                continue;
            }
            let Some(text) = msg.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
                continue;
            };
            match handle_text(&cfg, text) {
                Ok(Some(reply)) => telegram::send_message(&cfg, msg.chat.id, &reply)?,
                Ok(None) => {}
                Err(e) => {
                    let _ = telegram::send_message(
                        &cfg,
                        msg.chat.id,
                        &format!("⚠️ Не получилось: {e:#}"),
                    );
                }
            }
            handled += 1;
        }
        if handled >= 25 {
            break;
        }
    }
    st.save(&cfg.state_path)?;
    if handled > 0 {
        eprintln!("processed messages: {handled}");
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
