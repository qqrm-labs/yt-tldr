use crate::{
    state::{Pending, State},
    telegram::Update,
};
use anyhow::{Result, bail};

const MAX_PROVIDER_ATTEMPTS: u8 = 3;
const FAILURE_REPLY: &str = "⚠️ Не получилось обработать видео. Попробуйте отправить ссылку позже.";

#[derive(Debug)]
pub struct ProviderFailure(pub reqwest::StatusCode);
impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider HTTP {}", self.0)
    }
}
impl std::error::Error for ProviderFailure {}

fn retryable(error: &anyhow::Error) -> bool {
    if let Some(ProviderFailure(status)) = error.downcast_ref::<ProviderFailure>() {
        status.is_server_error() || status.as_u16() == 429 || status.as_u16() == 408
    } else {
        error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_timeout() || e.is_connect())
    }
}

fn complete(state: &mut State, update: &Update) {
    if let Some(message) = &update.message {
        state.mark_processed(&format!("{}:{}", message.chat.id, message.message_id));
    }
    state.pending = None;
    state.update_offset = state.update_offset.max(update.update_id.saturating_add(1));
}

/// Persist only metadata: Telegram retains the original update until completion.
/// Resuming a partial reply requires regeneration with an identical fingerprint.
pub fn process_update(
    state: &mut State,
    update: &Update,
    allowed: bool,
    mut analyze: impl FnMut(&str) -> Result<Option<String>>,
    mut send: impl FnMut(i64, &str) -> Result<()>,
    mut checkpoint: impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    if state
        .pending
        .as_ref()
        .is_some_and(|p| p.update_id != update.update_id)
    {
        bail!("finish the pending update before processing another");
    }
    let Some(message) = &update.message else {
        complete(state, update);
        return checkpoint(state);
    };
    let key = format!("{}:{}", message.chat.id, message.message_id);
    if !allowed
        || state.is_processed(&key)
        || message.text.as_deref().is_none_or(|t| t.trim().is_empty())
    {
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
        });
        checkpoint(state)?;
    }
    if state.pending.as_ref().unwrap().chunks.is_none() {
        let attempts = state.pending.as_ref().unwrap().attempts;
        let reply = if attempts >= MAX_PROVIDER_ATTEMPTS {
            Some(FAILURE_REPLY.to_string())
        } else {
            state.pending.as_mut().unwrap().attempts += 1;
            checkpoint(state)?; // A crash still consumes the reserved attempt.
            match analyze(message.text.as_deref().unwrap().trim()) {
                Ok(reply) => reply,
                Err(error) if retryable(&error) && attempts + 1 < MAX_PROVIDER_ATTEMPTS => {
                    bail!("temporary provider failure; update retained for the next run");
                }
                Err(_) => Some(FAILURE_REPLY.to_string()),
            }
        };
        let chunks = reply
            .map(|text| crate::telegram::message_chunks(&text))
            .unwrap_or_default();
        let hash = crate::state::fingerprint(&serde_json::to_vec(&chunks)?);
        let pending = state.pending.as_mut().unwrap();
        if pending.next_chunk > 0 && pending.reply_hash.as_ref() != Some(&hash) {
            bail!(
                "regenerated reply differs after partial delivery; operator reconciliation required"
            );
        }
        anyhow::ensure!(
            pending.next_chunk <= chunks.len(),
            "invalid persisted delivery cursor"
        );
        pending.reply_hash = Some(hash);
        pending.chunks = Some(chunks);
        checkpoint(state)?;
    }
    loop {
        let pending = state.pending.as_ref().unwrap();
        let chunks = pending.chunks.as_ref().unwrap();
        if pending.next_chunk >= chunks.len() {
            break;
        }
        send(message.chat.id, &chunks[pending.next_chunk])?;
        state.pending.as_mut().unwrap().next_chunk += 1;
        checkpoint(state)?;
    }
    complete(state, update);
    checkpoint(state)
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
                    |_| Err(ProviderFailure(reqwest::StatusCode::from_u16(status).unwrap()).into()),
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
                    |_| Err(ProviderFailure(reqwest::StatusCode::TOO_MANY_REQUESTS).into()),
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
            |_| Err(ProviderFailure(reqwest::StatusCode::TOO_MANY_REQUESTS).into()),
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
    fn partial_delivery_resumes_only_unsent_chunks_after_identical_regeneration() {
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
                |_| Err(ProviderFailure(reqwest::StatusCode::INTERNAL_SERVER_ERROR).into()),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(state.update_offset, 2);
        assert!(state.is_processed("42:1"));
    }
}
