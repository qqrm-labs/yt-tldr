use crate::{
    config::Config,
    gemini, groq,
    http::{self, Services},
    progress::Stage,
    runner::{self, Handlers},
    runtime::{Budget, BudgetExhausted, unix_time},
    state::{CheckpointFailure, ReconciliationRequired, State},
    telegram, youtube,
};
use anyhow::{Context, Result};

const HELP: &str = "🎬 Пришли ссылку на публичное YouTube-видео — получишь саммари с ключевыми тейками.\n\nСначала подтвержу получение, затем покажу этапы обработки. /status — состояние на момент обработки команды. Видео-файлы не поддерживаются; нужна ссылка.";
pub const TRUNCATION_WARNING: &str =
    "⚠️ Саммари неполное: транскрипт или ответ был обрезан по лимиту.";
const INVALID_INPUT: &str = "Не нашёл корректную ссылку на публичное YouTube-видео. Пришли обычную ссылку youtube.com/watch?v=…, youtu.be/… или youtube.com/shorts/…. Видео-файлы пока не поддерживаются.";

pub fn initialize_poll(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    state: &mut State,
    mut checkpoint: impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    cfg.validate_poll()?;
    let identity = telegram::doctor(cfg, services, budget)?;
    if state.bind_bot(identity.id)? {
        checkpoint(state).context(CheckpointFailure)?;
    }
    Ok(())
}

pub fn poll(cfg: &Config, services: &Services, budget: &Budget) -> Result<usize> {
    crate::checkpoint::require_hosted_configuration()?;
    let mut state = State::load(&cfg.state_path)?;
    let mut checkpoint = |state: &State| state.save(&cfg.state_path, budget.checkpoint_timeout()?);
    initialize_poll(cfg, services, budget, &mut state, &mut checkpoint)?;
    poll_cycle(cfg, services, budget, &mut state, checkpoint, 0)
}

#[cfg(test)]
pub fn poll_with_checkpoint(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    state: &mut State,
    checkpoint: impl FnMut(&State) -> Result<()>,
) -> Result<usize> {
    poll_cycle(cfg, services, budget, state, checkpoint, 0)
}

/// Initialization binds this state to the verified bot before this cycle runs.
/// Commands may be answered out of order, but never acknowledge a pending video.
pub fn poll_cycle(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    state: &mut State,
    mut checkpoint: impl FnMut(&State) -> Result<()>,
    poll_timeout_secs: u32,
) -> Result<usize> {
    cfg.validate_poll()?;
    anyhow::ensure!(
        state.bot_fingerprint.is_some(),
        "initialize_poll must bind the bot before polling"
    );
    let mut durable = |state: &State| checkpoint(state).context(CheckpointFailure);
    let mut handled = 0;
    'poll: while handled < 25 {
        if state
            .telegram_retry_not_before
            .is_some_and(|not_before| unix_time() < not_before)
        {
            anyhow::ensure!(
                !state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.awaiting_ack),
                ReconciliationRequired("unconfirmed Telegram delivery")
            );
            eprintln!("poll_deferred reason=telegram_retry_after");
            break;
        }
        if !budget.can_start_update() {
            eprintln!("poll_deferred reason=execution_budget");
            break;
        }
        let updates = match telegram::get_updates(
            cfg,
            services,
            budget,
            state.update_offset,
            poll_timeout_secs,
        ) {
            Ok(updates) => updates,
            Err(error) => {
                if let Some(rejection) = error.downcast_ref::<telegram::TelegramRejection>()
                    && rejection.error_code == 429
                {
                    state.record_telegram_throttle(rejection.retry_after.unwrap_or(60));
                    durable(state)?;
                }
                return Err(error);
            }
        };
        handle_controls(
            cfg,
            services,
            budget,
            state,
            &updates,
            poll_timeout_secs > 0,
            &mut durable,
        )?;
        anyhow::ensure!(
            !state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.awaiting_ack),
            ReconciliationRequired("unconfirmed Telegram delivery")
        );
        if state
            .next_retry_deadline()
            .is_some_and(|not_before| unix_time() < not_before)
        {
            eprintln!("poll_deferred reason=retry_after");
            return Ok(handled);
        }
        if let Some(pending) = &state.pending {
            anyhow::ensure!(
                updates
                    .iter()
                    .any(|update| update.update_id == pending.update_id),
                ReconciliationRequired("pending update is unavailable from Telegram")
            );
        }
        if updates.is_empty() {
            break;
        }
        for update in updates.into_iter().take(25 - handled) {
            if !budget.can_start_update() {
                eprintln!("poll_deferred reason=execution_budget");
                break 'poll;
            }
            let allowed = update
                .message
                .as_ref()
                .is_some_and(|message| cfg.chat_allowed(message.chat.id));
            let progress_enabled = allowed
                && update
                    .message
                    .as_ref()
                    .and_then(|message| youtube::extract_youtube_url(message.input_text()))
                    .is_some();
            let outcome = runner::process_update_with_progress(
                state,
                &update,
                allowed,
                progress_enabled,
                Handlers {
                    analyze: |text: &str, report: &mut dyn FnMut(Stage) -> Result<()>| {
                        handle_text(cfg, services, budget, text, report)
                    },
                    send: |chat, text: &str| {
                        let receipt = telegram::send_chunk(cfg, services, budget, chat, text)?;
                        eprintln!("delivery_confirmed message_id={}", receipt.message_id);
                        Ok(())
                    },
                    progress: |chat, message_id, text: &str| {
                        if let Some(message_id) = message_id {
                            telegram::edit_progress(cfg, services, budget, chat, message_id, text)?;
                            Ok(None)
                        } else {
                            telegram::send_chunk(cfg, services, budget, chat, text)
                                .map(|receipt| Some(receipt.message_id))
                        }
                    },
                    checkpoint: &mut durable,
                },
            );
            if let Err(error) = outcome
                && !error.is::<runner::TerminalDeliveryRejected>()
            {
                return Err(error);
            }
            handled += 1;
        }
    }
    Ok(handled)
}

fn command<'a>(cfg: &Config, text: &'a str) -> Option<&'a str> {
    let first = text.split_whitespace().next()?;
    let (name, target) = first
        .split_once('@')
        .map_or((first, None), |(name, target)| (name, Some(target)));
    if target.is_some_and(|target| {
        !cfg.expected_telegram_bot_username
            .as_deref()
            .is_some_and(|expected| target.eq_ignore_ascii_case(expected.trim_start_matches('@')))
    }) {
        return None;
    }
    matches!(name, "/start" | "/help" | "/status").then_some(name)
}

fn status_text(state: &State, serving: bool) -> String {
    let mode = if serving {
        "постоянный worker"
    } else {
        "разовый цикл обработки"
    };
    let detail = match &state.pending {
        Some(pending) if pending.awaiting_ack => Stage::NeedsReview.text(pending.attempts),
        Some(pending)
            if state
                .next_retry_deadline()
                .is_some_and(|when| when > unix_time()) =>
        {
            Stage::RetryScheduled.text(pending.attempts)
        }
        Some(pending) => pending.progress.stage.text(pending.attempts),
        None => "Активной задачи сейчас нет. Пришли ссылку на публичное YouTube-видео.".into(),
    };
    format!(
        "Состояние на момент обработки команды.\nРежим: {mode}.\n{detail}\n\nКоманды читаются между запросами к сервисам, поэтому ответ на /status может задержаться во время обработки видео."
    )
}

fn handle_controls(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    state: &mut State,
    updates: &[telegram::Update],
    serving: bool,
    checkpoint: &mut impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    if state
        .telegram_retry_not_before
        .is_some_and(|not_before| unix_time() < not_before)
    {
        return Ok(());
    }
    for update in updates {
        let Some(message) = &update.message else {
            continue;
        };
        if !cfg.chat_allowed(message.chat.id) {
            continue;
        }
        let Some(command) = command(cfg, message.input_text()) else {
            continue;
        };
        let key = format!("{}:{}", message.chat.id, message.message_id);
        if state.is_processed(&key) {
            continue;
        }
        let text = if command == "/status" {
            status_text(state, serving)
        } else {
            HELP.into()
        };
        // Control replies are best-effort, at most one attempt per command.
        // This record must not change the pending request or Telegram offset.
        state.mark_processed(&key);
        checkpoint(state)?;
        if let Err(error) = telegram::send_chunk(cfg, services, budget, message.chat.id, &text) {
            if let Some(rejection) = error.downcast_ref::<telegram::TelegramRejection>() {
                eprintln!(
                    "control_notification_failed api_code={}",
                    rejection.error_code
                );
                if rejection.error_code == 429 {
                    state.record_telegram_throttle(rejection.retry_after.unwrap_or(60));
                    state.unmark_processed(&key);
                    checkpoint(state)?;
                    return Ok(());
                }
            } else {
                eprintln!("control_notification_failed category=unconfirmed");
            }
        }
    }
    Ok(())
}

pub fn retryable_poll_error(error: &anyhow::Error, state: &State) -> bool {
    if error.is::<CheckpointFailure>()
        || error.is::<ReconciliationRequired>()
        || state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.awaiting_ack)
    {
        return false;
    }
    error.is::<runner::RetryLater>()
        || error.is::<BudgetExhausted>()
        || error.is::<telegram::SendNotAttempted>()
        || error.is::<telegram::TelegramTransport>()
        || error.is::<telegram::TelegramReadTransient>()
        || error
            .downcast_ref::<telegram::TelegramRejection>()
            .is_some_and(|rejection| rejection.error_code == 429)
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
    report: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<Option<String>> {
    if text.starts_with('/') {
        return Ok(Some(
            "Не знаю такую команду. /help — инструкция, /status — состояние обработки.".into(),
        ));
    }
    match youtube::extract_youtube_url(text) {
        Some(url) => process_video_with_progress(cfg, services, budget, &url, report).map(Some),
        None => Ok(Some(INVALID_INPUT.into())),
    }
}

pub fn process_video(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    video_url: &str,
) -> Result<String> {
    process_video_with_progress(cfg, services, budget, video_url, &mut |_| Ok(()))
}

fn process_video_with_progress(
    cfg: &Config,
    services: &Services,
    budget: &Budget,
    video_url: &str,
    report: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<String> {
    cfg.validate_process()?;
    let url = youtube::normalize_youtube_url(video_url)
        .ok_or_else(|| anyhow::anyhow!("expected a valid public YouTube video URL"))?;
    report(Stage::Transcribing)?;
    let Some(transcript) = gemini::transcribe(cfg, services, budget, &url)? else {
        return Ok("Не нашёл в этом видео речи — саммарить нечего 🤷".to_string());
    };
    report(Stage::Summarizing)?;
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
