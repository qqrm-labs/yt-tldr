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

/// Commit progress before acknowledging an update, and persist the prepared
/// reply before sending so a delivery retry never repeats provider work.
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
        .is_some_and(|p| p.update.update_id != update.update_id)
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
            update: update.clone(),
            attempts: 0,
            chunks: None,
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
        state.pending.as_mut().unwrap().chunks = Some(
            reply
                .map(|text| crate::telegram::message_chunks(&text))
                .unwrap_or_default(),
        );
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
    fn partial_delivery_resumes_only_unsent_chunks_without_provider_calls() {
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
            |_| panic!("summary must be reused"),
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
