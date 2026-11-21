use super::*;
use crate::{progress::Stage, state::CheckpointFailure};

fn me(username: &str, id: i64) -> Step {
    step(
        "/botfixture-token/getMe",
        json!({"ok":true,"result":{"id":id,"is_bot":true,"username":username}}),
    )
}

fn webhook() -> Step {
    step(
        "/botfixture-token/getWebhookInfo",
        json!({"ok":true,"result":{"url":""}}),
    )
}

fn status_update() -> Value {
    json!({"update_id":101,"message":{"message_id":8,"chat":{"id":42},"text":"/status"}})
}

#[test]
fn progress_intent_precedes_provider_and_done_follows_durable_result_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let accepted_path = cfg.state_path.clone();
    let provider_path = cfg.state_path.clone();
    let completed_path = cfg.state_path.clone();
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted().inspect(move |_| {
            let state = State::load(&accepted_path).unwrap();
            let pending = state.pending.unwrap();
            assert!(pending.progress.creation_attempted);
            assert!(pending.progress.message_id.is_none());
            assert_eq!(pending.attempts, 0);
            assert!(!pending.awaiting_ack);
        }),
        progress_edit("расшифровку"),
        step(GEMINI, transcript("STOP")).inspect(move |_| {
            let state = State::load(&provider_path).unwrap();
            assert_eq!(
                state.pending.as_ref().unwrap().progress.message_id,
                Some(55)
            );
            let raw = std::fs::read_to_string(&provider_path).unwrap();
            for private in [
                VIDEO,
                "youtu.be",
                "fixture-token",
                "chat_id",
                "Please summarize",
            ] {
                assert!(!raw.contains(private));
            }
        }),
        progress_edit("Готовлю саммари"),
        step(GROQ, summary("stop")),
        progress_edit("Отправляю результат"),
        accepted(),
        progress_edit("Результат отправлен").inspect(move |_| {
            let state = State::load(&completed_path).unwrap();
            let pending = state.pending.unwrap();
            assert!(pending.delivery_complete);
            assert!(!pending.awaiting_ack);
            assert_eq!(pending.next_chunk, 1);
        }),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
    assert_eq!(state.update_offset, 101);
    assert!(state.pending.is_none());
    fixture.finish();
}

#[test]
fn unknown_progress_receipt_is_not_resent_after_restart_and_does_not_block_result() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted().disconnect(),
        step(GEMINI, json!({"error":"busy"}))
            .status(503)
            .retry_after(3600),
        updates(),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")),
        accepted(),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    assert!(poll(&cfg, &fixture, &mut state).is_err());
    let mut restored = State::load(&cfg.state_path).unwrap();
    let pending = restored.pending.as_mut().unwrap();
    assert!(pending.progress.creation_attempted);
    assert!(pending.progress.message_id.is_none());
    assert!(!pending.awaiting_ack);
    pending.retry_not_before = Some(unix_time().saturating_sub(1));
    assert_eq!(poll(&cfg, &fixture, &mut restored).unwrap(), 1);
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == SEND)
            .count(),
        2
    );
    assert!(!requests.iter().any(|request| request.path == EDIT));
}

#[test]
fn failed_progress_edit_never_becomes_a_failed_or_duplicate_result() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        step(EDIT, json!({"ok":false,"error_code":500})).status(500),
        step(GEMINI, transcript("STOP")),
        progress_edit("Готовлю саммари"),
        step(GROQ, summary("stop")),
        progress_edit("Отправляю результат"),
        accepted(),
        progress_edit("Результат отправлен"),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
    assert!(state.pending.is_none());
    fixture.finish();
}

#[test]
fn restart_after_durable_last_receipt_only_finishes_status_without_regeneration() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        progress_edit("расшифровку"),
        step(GEMINI, transcript("STOP")),
        progress_edit("Готовлю саммари"),
        step(GROQ, summary("stop")),
        progress_edit("Отправляю результат"),
        accepted(),
        updates(),
        progress_edit("Результат отправлен"),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    let error = app::poll_with_checkpoint(
        &cfg,
        &fixture.services,
        &Budget::new(POLL_BUDGET),
        &mut state,
        |state| {
            state.write_local(&cfg.state_path)?;
            if state.pending.as_ref().is_some_and(|pending| {
                pending.delivery_complete && pending.progress.stage == Stage::Delivering
            }) {
                anyhow::bail!("fixture crash after durable final receipt");
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.is::<CheckpointFailure>());
    assert!(!app::retryable_poll_error(&error, &state));
    let mut restored = State::load(&cfg.state_path).unwrap();
    assert!(restored.pending.as_ref().unwrap().delivery_complete);
    assert_eq!(poll(&cfg, &fixture, &mut restored).unwrap(), 1);
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == GEMINI)
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == SEND
                && request.body["text"].as_str().unwrap().contains("TL;DR:"))
            .count(),
        1
    );
}

#[test]
fn provider_retry_status_and_status_command_do_not_acknowledge_the_pending_video() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let command_path = cfg.state_path.clone();
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        progress_edit("расшифровку"),
        step(GEMINI, json!({"error":"busy"}))
            .status(429)
            .retry_after(3600),
        progress_edit("Повторю автоматически"),
        step(
            UPDATES,
            json!({"ok":true,"result":[update(),status_update()]}),
        )
        .inspect(|request| assert_eq!(request.body["offset"], 0)),
        step(SEND, json!({"ok":true,"result":{"message_id":80}})).inspect(move |request| {
            assert!(
                request.body["text"]
                    .as_str()
                    .unwrap()
                    .contains("Повторю автоматически")
            );
            let state = State::load(&command_path).unwrap();
            assert_eq!(state.update_offset, 0);
            assert_eq!(state.pending.as_ref().unwrap().update_id, 100);
        }),
        step(
            UPDATES,
            json!({"ok":true,"result":[update(),status_update()]}),
        )
        .inspect(|request| assert_eq!(request.body["offset"], 0)),
    ]);
    let mut state = fixture_state();
    assert!(poll(&cfg, &fixture, &mut state).is_err());
    assert!(state.telegram_retry_not_before.is_none());
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    assert_eq!(state.update_offset, 0);
    assert!(state.is_processed("42:8"));
    fixture.finish();
}

#[test]
fn telegram_progress_throttle_suppresses_outbound_calls_until_its_separate_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        step(
            EDIT,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":60}}),
        )
        .status(429),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")),
        updates(),
        progress_edit("расшифровку"),
        step(GEMINI, transcript("STOP")),
        progress_edit("Готовлю саммари"),
        step(GROQ, summary("stop")),
        progress_edit("Отправляю результат"),
        accepted(),
        progress_edit("Результат отправлен"),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(app::retryable_poll_error(&error, &state));
    assert!(state.telegram_retry_not_before.unwrap() > unix_time());
    assert!(!state.pending.as_ref().unwrap().awaiting_ack);
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    let mut restored = State::load(&cfg.state_path).unwrap();
    restored.telegram_retry_not_before = Some(unix_time().saturating_sub(1));
    assert_eq!(poll(&cfg, &fixture, &mut restored).unwrap(), 1);
    fixture.finish();
}

#[test]
fn confirmed_control_throttle_retries_the_same_command_after_cooldown() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        step(UPDATES, json!({"ok":true,"result":[status_update()]})),
        step(
            SEND,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":60}}),
        )
        .status(429),
        step(UPDATES, json!({"ok":true,"result":[status_update()]})),
        step(SEND, json!({"ok":true,"result":{"message_id":80}})),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    assert_eq!(state.update_offset, 0);
    assert!(!state.is_processed("42:8"));
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    let mut restored = State::load(&cfg.state_path).unwrap();
    restored.telegram_retry_not_before = Some(unix_time().saturating_sub(1));
    assert_eq!(poll(&cfg, &fixture, &mut restored).unwrap(), 1);
    assert_eq!(restored.update_offset, 102);
    assert!(restored.is_processed("42:8"));
    fixture.finish();
}

#[test]
fn invalid_text_and_nontext_messages_get_clear_feedback_without_provider_calls() {
    for message in [
        json!({"message_id":7,"chat":{"id":42},"text":"https://notyoutube.com/watch?v=jNQXAC9IVRw"}),
        json!({"message_id":7,"chat":{"id":42},"video":{"file_id":"private-video-file"}}),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = cfg(&directory);
        let fixture = Fixture::new(vec![
            step(
                UPDATES,
                json!({"ok":true,"result":[{"update_id":100,"message":message}]}),
            ),
            accepted().inspect(|request| {
                assert!(
                    request.body["text"]
                        .as_str()
                        .unwrap()
                        .contains("Не нашёл корректную ссылку")
                )
            }),
            empty_updates(),
        ]);
        let mut state = fixture_state();
        assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
        fixture.finish();
    }
}

#[test]
fn all_telegram_entry_points_reject_shared_or_unexpected_bot_before_side_effects() {
    for username in ["github_weekly_digest_bot", "unexpected_fixture_bot"] {
        for operation in ["doctor", "setup", "poll"] {
            let directory = tempfile::tempdir().unwrap();
            let cfg = cfg(&directory);
            let fixture = Fixture::new(vec![me(username, 123456789)]);
            let mut state = State::default();
            let budget = Budget::new(POLL_BUDGET);
            let result = match operation {
                "doctor" => app::doctor(&cfg, &fixture.services, &budget).map(|_| ()),
                "setup" => telegram::setup_commands(&cfg, &fixture.services, &budget),
                _ => app::initialize_poll(&cfg, &fixture.services, &budget, &mut state, |_| {
                    panic!("identity mismatch must precede writes")
                }),
            };
            assert!(result.is_err());
            assert!(state.bot_fingerprint.is_none());
            fixture.finish();
        }
    }
}

#[test]
fn state_identity_binding_accepts_fresh_state_and_refuses_reusing_nonempty_or_other_bot_state() {
    for variant in ["fresh", "unbound_nonempty", "other_bot"] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = cfg(&directory);
        let fixture = Fixture::new(vec![me("fixture_bot", 123456789), webhook()]);
        let mut state = State::default();
        if variant == "unbound_nonempty" {
            state.update_offset = 999;
        }
        if variant == "other_bot" {
            state.bind_bot(987654321).unwrap();
        }
        let before = serde_json::to_string(&state).unwrap();
        let result = app::initialize_poll(
            &cfg,
            &fixture.services,
            &Budget::new(POLL_BUDGET),
            &mut state,
            |state| state.write_local(&cfg.state_path),
        );
        if variant == "fresh" {
            result.unwrap();
            let raw = std::fs::read_to_string(&cfg.state_path).unwrap();
            assert!(state.bot_fingerprint.is_some());
            assert!(!raw.contains("123456789"));
        } else {
            assert!(result.is_err());
            assert_eq!(serde_json::to_string(&state).unwrap(), before);
            assert!(!std::path::Path::new(&cfg.state_path).exists());
        }
        fixture.finish();
    }
}

#[test]
fn telegram_read_5xx_is_retryable_but_final_send_5xx_keeps_the_unknown_intent() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        step(UPDATES, json!({}))
            .status(502)
            .raw_body(b"private upstream response"),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(error.is::<telegram::TelegramReadTransient>());
    assert!(app::retryable_poll_error(&error, &state));
    assert!(!format!("{error:#}").contains("private"));
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    fixture.finish();
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        progress_edit("расшифровку"),
        step(GEMINI, transcript("STOP")),
        progress_edit("Готовлю саммари"),
        step(GROQ, summary("stop")),
        progress_edit("Отправляю результат"),
        step(SEND, json!({}))
            .status(502)
            .raw_body(b"private upstream response"),
        progress_edit("Не удалось подтвердить"),
    ]);
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(state.pending.as_ref().unwrap().awaiting_ack);
    assert!(!app::retryable_poll_error(&error, &state));
    fixture.finish();
}

#[test]
fn terminal_provider_error_reports_failure_only_after_the_error_reply_is_acknowledged() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let checkpoint_path = cfg.state_path.clone();
    let fixture = Fixture::new(vec![
        updates(),
        progress_accepted(),
        progress_edit("расшифровку"),
        step(GEMINI, json!({"error":"denied"})).status(400),
        progress_edit("сообщение об ошибке"),
        accepted(),
        progress_edit("Не получилось обработать видео").inspect(move |_| {
            let state = State::load(&checkpoint_path).unwrap();
            let pending = state.pending.unwrap();
            assert!(pending.result_failed && pending.delivery_complete && !pending.awaiting_ack);
        }),
        empty_updates(),
    ]);
    let mut state = fixture_state();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
    fixture.finish();
}
