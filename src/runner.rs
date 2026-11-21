use crate::{
    http::ProviderError,
    progress::{Progress, Stage},
    runtime::{BudgetExhausted, unix_time},
    state::{Pending, ReconciliationRequired, State},
    telegram::{SendNotAttempted, TelegramRejection, Update},
};
use anyhow::{Context, Result, bail};

const MAX_PROVIDER_ATTEMPTS: u8 = 3;
const FAILURE_REPLY: &str = "⚠️ Не получилось обработать видео после ограниченного числа попыток. Попробуйте отправить ссылку позже.";

#[derive(Debug)]
pub struct RetryLater;
impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("temporary failure; update retained for an automatic retry")
    }
}
impl std::error::Error for RetryLater {}

#[derive(Debug)]
pub struct TerminalDeliveryRejected(pub u16);
impl std::fmt::Display for TerminalDeliveryRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "result delivery rejected by Telegram: API {}", self.0)
    }
}
impl std::error::Error for TerminalDeliveryRejected {}

#[derive(Debug)]
struct ProgressCheckpointFailure;
impl std::fmt::Display for ProgressCheckpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("progress checkpoint failed; refusing further delivery")
    }
}
impl std::error::Error for ProgressCheckpointFailure {}

pub struct Handlers<A, S, P, C> {
    pub analyze: A,
    pub send: S,
    pub progress: P,
    pub checkpoint: C,
}

fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ProviderError>()
        .is_some_and(ProviderError::retryable)
}

fn complete(state: &mut State, update: &Update) {
    if let Some(message) = &update.message {
        state.mark_processed(&format!("{}:{}", message.chat.id, message.message_id));
    }
    state.pending = None;
    state.update_offset = state.update_offset.max(update.update_id.saturating_add(1));
}

fn publish_progress(
    state: &mut State,
    chat_id: i64,
    stage: Stage,
    notify: &mut impl FnMut(i64, Option<i64>, &str) -> Result<Option<i64>>,
    checkpoint: &mut impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    let throttled = state
        .telegram_retry_not_before
        .is_some_and(|not_before| unix_time() < not_before);
    let pending = state
        .pending
        .as_mut()
        .expect("progress belongs to a pending request");
    if !pending.progress.enabled {
        return Ok(());
    }
    let changed = pending.progress.stage != stage;
    pending.progress.stage = stage;
    let create = !pending.progress.creation_attempted;
    if create && !throttled {
        // Unlike result delivery, a lost progress receipt is allowed to remain
        // unknown. Persisting this attempt prevents a duplicate accepted message.
        pending.progress.creation_attempted = true;
    }
    let message_id = pending.progress.message_id;
    if !create && !changed {
        return Ok(());
    }
    let text = if stage == Stage::Delivering && pending.result_failed {
        "📨 Отправляю сообщение об ошибке обработки.".to_string()
    } else {
        stage.text(pending.attempts)
    };
    checkpoint(state).context(ProgressCheckpointFailure)?;
    if throttled || (!create && message_id.is_none()) {
        return Ok(());
    }
    match notify(chat_id, message_id, &text) {
        Ok(receipt) => {
            if create && let Some(message_id) = receipt.filter(|id| *id > 0) {
                state.pending.as_mut().unwrap().progress.message_id = Some(message_id);
                checkpoint(state).context(ProgressCheckpointFailure)?;
            }
        }
        Err(error) => {
            if let Some(rejection) = error.downcast_ref::<TelegramRejection>() {
                eprintln!(
                    "status_notification_failed api_code={}",
                    rejection.error_code
                );
                if rejection.error_code == 429 {
                    state.record_telegram_throttle(rejection.retry_after.unwrap_or(60));
                    if create {
                        state.pending.as_mut().unwrap().progress.creation_attempted = false;
                    }
                    checkpoint(state).context(ProgressCheckpointFailure)?;
                }
            } else {
                eprintln!("status_notification_failed category=unconfirmed");
            }
            // Notification failure never sets result awaiting_ack, consumes a
            // provider attempt, or causes a fresh status send on restart.
        }
    }
    Ok(())
}

/// Final messages retain their strict intent/receipt protocol. Progress is a
/// separate at-most-once creation followed by harmless, repeatable edits.
pub fn process_update_with_progress<A, S, P, C>(
    state: &mut State,
    update: &Update,
    allowed: bool,
    progress_enabled: bool,
    handlers: Handlers<A, S, P, C>,
) -> Result<()>
where
    A: FnMut(&str, &mut dyn FnMut(Stage) -> Result<()>) -> Result<Option<String>>,
    S: FnMut(i64, &str) -> Result<()>,
    P: FnMut(i64, Option<i64>, &str) -> Result<Option<i64>>,
    C: FnMut(&State) -> Result<()>,
{
    let Handlers {
        mut analyze,
        mut send,
        mut progress,
        mut checkpoint,
    } = handlers;
    anyhow::ensure!(
        !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.awaiting_ack),
        ReconciliationRequired("unconfirmed Telegram delivery")
    );
    anyhow::ensure!(
        !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.update_id != update.update_id),
        ReconciliationRequired("finish the pending update before processing another")
    );
    if state
        .pending
        .as_ref()
        .and_then(|pending| pending.retry_not_before)
        .is_some_and(|not_before| unix_time() < not_before)
    {
        bail!(RetryLater);
    }
    let Some(message) = &update.message else {
        complete(state, update);
        return checkpoint(state);
    };
    let key = format!("{}:{}", message.chat.id, message.message_id);
    if !allowed || state.is_processed(&key) {
        complete(state, update);
        return checkpoint(state);
    }
    if state.pending.is_none() {
        state.pending = Some(Pending {
            update_id: update.update_id,
            attempts: 0,
            chunks: None,
            reply_hash: None,
            next_chunk: 0,
            awaiting_ack: false,
            retry_not_before: None,
            progress: Progress {
                enabled: progress_enabled,
                ..Progress::default()
            },
            result_failed: false,
            delivery_complete: false,
        });
        checkpoint(state)?;
    }
    if state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.progress.enabled && !pending.progress.creation_attempted)
    {
        publish_progress(
            state,
            message.chat.id,
            Stage::Accepted,
            &mut progress,
            &mut checkpoint,
        )?;
    }
    if !state.pending.as_ref().unwrap().delivery_complete {
        if state.pending.as_ref().unwrap().chunks.is_none() {
            let attempts = state.pending.as_ref().unwrap().attempts;
            let reply = if attempts >= MAX_PROVIDER_ATTEMPTS {
                state.pending.as_mut().unwrap().result_failed = true;
                Some(FAILURE_REPLY.to_string())
            } else {
                state.pending.as_mut().unwrap().attempts += 1;
                checkpoint(state)?; // A crash still consumes the reserved attempt.
                let result = {
                    let mut report = |stage| {
                        publish_progress(
                            state,
                            message.chat.id,
                            stage,
                            &mut progress,
                            &mut checkpoint,
                        )
                    };
                    analyze(message.input_text().trim(), &mut report)
                };
                match result {
                    Ok(reply) => reply,
                    Err(error) if error.is::<ProgressCheckpointFailure>() => return Err(error),
                    Err(error) if error.is::<BudgetExhausted>() => {
                        publish_progress(
                            state,
                            message.chat.id,
                            Stage::RetryScheduled,
                            &mut progress,
                            &mut checkpoint,
                        )?;
                        return Err(error);
                    }
                    Err(error) if retryable(&error) && attempts + 1 < MAX_PROVIDER_ATTEMPTS => {
                        let retry_at = error
                            .downcast_ref::<ProviderError>()
                            .and_then(ProviderError::retry_not_before);
                        let pending = state.pending.as_mut().unwrap();
                        pending.retry_not_before = pending.retry_not_before.max(retry_at);
                        checkpoint(state)?;
                        publish_progress(
                            state,
                            message.chat.id,
                            Stage::RetryScheduled,
                            &mut progress,
                            &mut checkpoint,
                        )?;
                        eprintln!("analysis_retry {error}");
                        bail!(RetryLater);
                    }
                    Err(error) => {
                        state.pending.as_mut().unwrap().result_failed = true;
                        if let Some(error) = error.downcast_ref::<ProviderError>() {
                            eprintln!("analysis_failed {error}");
                        } else {
                            eprintln!("analysis_failed category=configuration_or_contract");
                        }
                        Some(FAILURE_REPLY.to_string())
                    }
                }
            };
            let chunks = reply
                .map(|text| crate::telegram::message_chunks(&text))
                .unwrap_or_default();
            let hash = crate::state::fingerprint(&serde_json::to_vec(&chunks)?);
            let pending = state.pending.as_mut().unwrap();
            anyhow::ensure!(
                pending.next_chunk == 0 || pending.reply_hash.as_ref() == Some(&hash),
                ReconciliationRequired("regenerated reply differs after partial delivery")
            );
            anyhow::ensure!(
                pending.next_chunk <= chunks.len(),
                ReconciliationRequired("invalid persisted delivery cursor")
            );
            pending.reply_hash = Some(hash);
            pending.chunks = Some(chunks);
            checkpoint(state)?;
        }
        publish_progress(
            state,
            message.chat.id,
            Stage::Delivering,
            &mut progress,
            &mut checkpoint,
        )?;
        loop {
            let pending = state.pending.as_ref().unwrap();
            let chunks = pending.chunks.as_ref().unwrap();
            if pending.next_chunk >= chunks.len() {
                state.pending.as_mut().unwrap().delivery_complete = true;
                checkpoint(state)?;
                break;
            }
            if state
                .next_retry_deadline()
                .is_some_and(|not_before| unix_time() < not_before)
            {
                bail!(RetryLater);
            }
            let chunk = chunks[pending.next_chunk].clone();
            state.pending.as_mut().unwrap().awaiting_ack = true;
            checkpoint(state)?;
            if let Err(error) = send(message.chat.id, &chunk) {
                let rejection = error.downcast_ref::<TelegramRejection>();
                if rejection.is_some() || error.is::<SendNotAttempted>() {
                    let retry_at = rejection
                        .filter(|rejection| rejection.error_code == 429)
                        .map(|rejection| {
                            state.record_telegram_throttle(rejection.retry_after.unwrap_or(60))
                        });
                    let pending = state.pending.as_mut().unwrap();
                    pending.awaiting_ack = false;
                    pending.retry_not_before = retry_at;
                    checkpoint(state)?;
                    if let Some(rejection) = rejection {
                        eprintln!(
                            "delivery_failure confirmed_rejection={} retry_after={}",
                            rejection.error_code,
                            rejection.retry_after.unwrap_or(0)
                        );
                        if matches!(rejection.error_code, 400 | 403) {
                            let code = rejection.error_code;
                            publish_progress(
                                state,
                                message.chat.id,
                                Stage::DeliveryRejected,
                                &mut progress,
                                &mut checkpoint,
                            )?;
                            complete(state, update);
                            checkpoint(state)?;
                            return Err(TerminalDeliveryRejected(code).into());
                        }
                    }
                    let stage = if rejection.is_some_and(|rejection| rejection.error_code != 429) {
                        Stage::ConfigurationError
                    } else {
                        Stage::RetryScheduled
                    };
                    publish_progress(
                        state,
                        message.chat.id,
                        stage,
                        &mut progress,
                        &mut checkpoint,
                    )?;
                } else {
                    publish_progress(
                        state,
                        message.chat.id,
                        Stage::NeedsReview,
                        &mut progress,
                        &mut checkpoint,
                    )?;
                }
                return Err(error);
            }
            let pending = state.pending.as_mut().unwrap();
            pending.next_chunk += 1;
            pending.awaiting_ack = false;
            pending.retry_not_before = None;
            // The last receipt and this marker become durable atomically.
            pending.delivery_complete =
                pending.next_chunk >= pending.chunks.as_ref().unwrap().len();
            checkpoint(state)?;
        }
    }
    let stage = if state.pending.as_ref().unwrap().result_failed {
        Stage::Failed
    } else {
        Stage::Completed
    };
    publish_progress(
        state,
        message.chat.id,
        stage,
        &mut progress,
        &mut checkpoint,
    )?;
    complete(state, update);
    checkpoint(state)
}

#[cfg(test)]
pub fn process_update(
    state: &mut State,
    update: &Update,
    allowed: bool,
    mut analyze: impl FnMut(&str) -> Result<Option<String>>,
    send: impl FnMut(i64, &str) -> Result<()>,
    checkpoint: impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    process_update_with_progress(
        state,
        update,
        allowed,
        false,
        Handlers {
            analyze: |text: &str, _: &mut dyn FnMut(Stage) -> Result<()>| analyze(text),
            send,
            progress: |_: i64, _: Option<i64>, _: &str| Ok(None),
            checkpoint,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::{Chat, Message};
    fn update(id: u64) -> Update {
        Update {
            update_id: id,
            message: Some(Message {
                message_id: id as i64,
                chat: Chat { id: 42 },
                text: Some("video".into()),
                caption: None,
            }),
        }
    }
    fn restored(state: &State) -> State {
        serde_json::from_slice(&serde_json::to_vec(state).unwrap()).unwrap()
    }
    #[test]
    fn unauthorized_updates_never_spend_provider_quota() {
        let mut state = State::default();
        process_update(
            &mut state,
            &update(1),
            false,
            |_| panic!("provider called"),
            |_, _| panic!("send called"),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(state.update_offset, 2);
    }
    #[test]
    fn provider_429_and_500_retry_without_ack_then_succeed() {
        let mut state = State::default();
        for status in [429, 500] {
            assert!(
                process_update(
                    &mut state,
                    &update(1),
                    true,
                    |_| Err(ProviderError::http(
                        "fixture",
                        "analyze",
                        reqwest::StatusCode::from_u16(status).unwrap()
                    )
                    .into()),
                    |_, _| panic!("no reply yet"),
                    |_| Ok(())
                )
                .is_err()
            );
            state = restored(&state);
            assert_eq!(state.update_offset, 0);
            assert!(!state.is_processed("42:1"));
        }
        process_update(
            &mut state,
            &update(1),
            true,
            |_| Ok(Some("summary".into())),
            |_, text| {
                assert_eq!(text, "summary");
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(state.update_offset, 2);
        assert!(state.is_processed("42:1"));
    }
    #[test]
    fn provider_retry_budget_is_persistent_and_terminal_reply_is_safe() {
        let mut state = State::default();
        for _ in 0..2 {
            assert!(
                process_update(
                    &mut state,
                    &update(1),
                    true,
                    |_| Err(ProviderError::http(
                        "fixture",
                        "analyze",
                        reqwest::StatusCode::TOO_MANY_REQUESTS
                    )
                    .into()),
                    |_, _| Ok(()),
                    |_| Ok(())
                )
                .is_err()
            );
            state = restored(&state);
        }
        process_update(
            &mut state,
            &update(1),
            true,
            |_| {
                Err(ProviderError::http(
                    "fixture",
                    "analyze",
                    reqwest::StatusCode::TOO_MANY_REQUESTS,
                )
                .into())
            },
            |_, text| {
                assert_eq!(text, FAILURE_REPLY);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(state.update_offset, 2);
    }
    #[test]
    fn ambiguous_partial_delivery_requires_reconciliation_before_resuming() {
        let mut state = State::default();
        let mut sends = 0;
        assert!(
            process_update(
                &mut state,
                &update(1),
                true,
                |_| Ok(Some("a".repeat(4000))),
                |_, _| {
                    sends += 1;
                    if sends == 2 {
                        bail!("send failed")
                    }
                    Ok(())
                },
                |_| Ok(())
            )
            .is_err()
        );
        state = restored(&state);
        assert_eq!(state.pending.as_ref().unwrap().next_chunk, 1);
        assert!(
            process_update(
                &mut state,
                &update(1),
                true,
                |_| panic!("provider before reconciliation"),
                |_, _| panic!("duplicate send"),
                |_| Ok(())
            )
            .is_err()
        );
        // The operator has verified that the timed-out chunk was not delivered.
        state.pending.as_mut().unwrap().awaiting_ack = false;
        process_update(
            &mut state,
            &update(1),
            true,
            |_| Ok(Some("a".repeat(4000))),
            |_, chunk| {
                assert_eq!(chunk.len(), 100);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(state.update_offset, 2);
    }
    #[test]
    fn checkpoint_failure_prevents_provider_calls() {
        assert!(
            process_update(
                &mut State::default(),
                &update(1),
                true,
                |_| panic!("unsafe provider call"),
                |_, _| panic!("unsafe send"),
                |_| bail!("disk failure")
            )
            .is_err()
        );
    }
    #[test]
    fn changed_reply_after_partial_delivery_fails_before_sending() {
        let mut state = State::default();
        let mut sends = 0;
        assert!(
            process_update(
                &mut state,
                &update(1),
                true,
                |_| Ok(Some("a".repeat(4000))),
                |_, _| {
                    sends += 1;
                    if sends == 2 {
                        bail!("transport failed")
                    }
                    Ok(())
                },
                |_| Ok(())
            )
            .is_err()
        );
        state = restored(&state);
        state.pending.as_mut().unwrap().awaiting_ack = false;
        let error = process_update(
            &mut state,
            &update(1),
            true,
            |_| Ok(Some("changed".into())),
            |_, _| panic!("must not mix replies"),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("reconciliation"));
        assert_eq!(state.update_offset, 0);
        assert_eq!(state.pending.as_ref().unwrap().next_chunk, 1);
    }

    #[test]
    fn persisted_state_contains_no_plaintext_message_chat_id_or_reply() {
        let mut update = update(1);
        let message = update.message.as_mut().unwrap();
        message.chat.id = -1009876543210;
        message.text = Some("private user message".into());
        let mut state = State::default();
        assert!(
            process_update(
                &mut state,
                &update,
                true,
                |_| Ok(Some("private generated reply".into())),
                |_, _| bail!("stop before delivery"),
                |state| {
                    let json = serde_json::to_string(state)?;
                    for secret in [
                        "private user message",
                        "private generated reply",
                        "-1009876543210",
                        "chat",
                        "text",
                        "chunks",
                    ] {
                        assert!(!json.contains(secret));
                    }
                    Ok(())
                }
            )
            .is_err()
        );
        // A verified no-delivery result is required before retrying.
        state.pending.as_mut().unwrap().awaiting_ack = false;
        process_update(
            &mut state,
            &update,
            true,
            |_| panic!("in-memory reply exists"),
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("-1009876543210"));
        assert!(!json.contains("private"));
    }
    #[test]
    fn earlier_completed_update_survives_later_failure() {
        let mut state = State::default();
        process_update(
            &mut state,
            &update(1),
            true,
            |_| Ok(None),
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        state = restored(&state);
        assert!(
            process_update(
                &mut state,
                &update(2),
                true,
                |_| Err(ProviderError::http(
                    "fixture",
                    "analyze",
                    reqwest::StatusCode::INTERNAL_SERVER_ERROR
                )
                .into()),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(state.update_offset, 2);
        assert!(state.is_processed("42:1"));
    }

    #[test]
    fn failed_ack_checkpoint_preserves_remote_intent_and_blocks_restart() {
        let mut state = State::default();
        let mut remote = Vec::new();
        let mut sent = false;
        assert!(
            process_update(
                &mut state,
                &update(1),
                true,
                |_| Ok(Some("summary".into())),
                |_, _| {
                    sent = true;
                    Ok(())
                },
                |state| {
                    if state.pending.as_ref().is_some_and(|p| p.next_chunk == 1) {
                        bail!("remote ack checkpoint unavailable")
                    }
                    remote = serde_json::to_vec(state)?;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(sent);
        let mut restored: State = serde_json::from_slice(&remote).unwrap();
        assert!(restored.pending.as_ref().unwrap().awaiting_ack);
        assert!(
            process_update(
                &mut restored,
                &update(1),
                true,
                |_| panic!("must not regenerate"),
                |_, _| panic!("must not resend"),
                |_| Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn failed_intent_checkpoint_prevents_send() {
        assert!(
            process_update(
                &mut State::default(),
                &update(1),
                true,
                |_| Ok(Some("summary".into())),
                |_, _| panic!("intent was not durable"),
                |state| {
                    if state.pending.as_ref().is_some_and(|p| p.awaiting_ack) {
                        bail!("checkpoint offline")
                    }
                    Ok(())
                }
            )
            .is_err()
        );
    }
}
