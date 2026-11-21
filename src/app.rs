use crate::{
    config::Config,
    gemini, groq,
    http::{self, Services},
    runner,
    runtime::{Budget, unix_time},
    state::State,
    telegram, youtube,
};
use anyhow::Result;

const HELP: &str = "🎬 Кидай ссылку на публичное YouTube-видео (обычную, youtu.be или /shorts/) — пришлю саммари с ключевыми тейками.\n\nБот запускается по расписанию GitHub Actions: запуск и обработка могут задержаться. Фиксированного времени ответа нет.";
pub const TRUNCATION_WARNING: &str =
    "⚠️ Саммари неполное: транскрипт или ответ был обрезан по лимиту.";

pub fn poll(cfg: &Config, services: &Services, budget: &Budget) -> Result<usize> {
    crate::checkpoint::require_hosted_configuration()?;
    let mut state = State::load(&cfg.state_path)?;
    poll_with_checkpoint(cfg, services, budget, &mut state, |state| {
        state.save(&cfg.state_path, budget.checkpoint_timeout()?)
    })
}

/// The same orchestration is exercised by local HTTP fixtures. Only the durable
/// checkpoint boundary is injectable; production endpoints are never configurable.
pub fn poll_with_checkpoint(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    state: &mut State,
    mut checkpoint: impl FnMut(&State) -> Result<()>,
) -> Result<usize> {
    cfg.validate_poll()?;
    anyhow::ensure!(
        !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.awaiting_ack),
        "unconfirmed Telegram delivery; operator reconciliation required"
    );
    if state
        .pending
        .as_ref()
        .and_then(|pending| pending.retry_not_before)
        .is_some_and(|not_before| unix_time() < not_before)
    {
        eprintln!("poll_deferred reason=retry_after");
        return Ok(0);
    }
    let mut handled = 0;
    'poll: while handled < 25 {
        if !budget.can_start_update() {
            eprintln!("poll_deferred reason=execution_budget");
            break;
        }
        let updates = telegram::get_updates(cfg, services, budget, state.update_offset)?;
        if updates.is_empty() {
            anyhow::ensure!(
                state.pending.is_none(),
                "pending update is unavailable from Telegram; operator reconciliation required"
            );
            break;
        }
        for update in updates.into_iter().take(25 - handled) {
            if !budget.can_start_update() {
                eprintln!("poll_deferred reason=execution_budget");
                break 'poll;
            }
            runner::process_update(
                state,
                &update,
                update
                    .message
                    .as_ref()
                    .is_some_and(|message| cfg.chat_allowed(message.chat.id)),
                |text| handle_text(cfg, services, budget, text),
                |chat, text| {
                    let receipt = telegram::send_chunk(cfg, services, budget, chat, text)?;
                    eprintln!("delivery_confirmed message_id={}", receipt.message_id);
                    Ok(())
                },
                &mut checkpoint,
            )?;
            handled += 1;
        }
    }
    Ok(handled)
}

pub fn doctor(cfg: &Config, services: &Services, budget: &Budget) -> Result<String> {
    cfg.validate_poll()?;
    let telegram = telegram::doctor(cfg, services, budget)?;
    http::check_models(cfg, services, budget)?;
    Ok(format!(
        "Telegram bot: @{}\nhttps://t.me/{}\nAllowed chats: {}\nWebhook: disabled\nGemini model: {} (metadata ready)\nGroq model: {} (metadata ready)\nVideo processing and actual Telegram delivery each require their own separate check.",
        telegram.username,
        telegram.username,
        cfg.allowed_chat_count(),
        cfg.gemini_model,
        cfg.groq_model
    ))
}

fn handle_text(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    text: &str,
) -> Result<Option<String>> {
    if text == "/start" || text == "/help" {
        return Ok(Some(HELP.to_string()));
    }
    if text.starts_with('/') {
        return Ok(Some(
            "Не знаю такую команду. Просто кинь ссылку на YouTube.".to_string(),
        ));
    }
    match youtube::extract_youtube_url(text) {
        Some(url) => Ok(Some(process_video(cfg, services, budget, &url)?)),
        None => Ok(None),
    }
}

pub fn process_video(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    video_url: &str,
) -> Result<String> {
    cfg.validate_process()?;
    let url = youtube::normalize_youtube_url(video_url)
        .ok_or_else(|| anyhow::anyhow!("expected a valid public YouTube video URL"))?;
    let Some(transcript) = gemini::transcribe(cfg, services, budget, &url)? else {
        return Ok("Не нашёл в этом видео речи — саммарить нечего 🤷".to_string());
    };
    let summary = groq::summarize(cfg, services, budget, &transcript.text)?;
    let mut out = String::new();
    if let Some(title) = &transcript.title {
        out.push_str(&format!("🎬 {title}\n\n"));
    }
    out.push_str(&summary.text);
    if transcript.truncated || summary.truncated {
        out.push_str("\n\n");
        out.push_str(TRUNCATION_WARNING);
    }
    Ok(out)
}
