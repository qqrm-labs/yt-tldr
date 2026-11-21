//! Local HTTP contracts exercise the real adapters and orchestration. No fixture
//! can select a production URL or use credentials from the environment.
use crate::{
    app,
    config::Config,
    http::{ProviderError, Services},
    runtime::{Budget, POLL_BUDGET, unix_time},
    state::State,
    telegram,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const VIDEO: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
const GEMINI: &str = "/models/gemini-fixture:generateContent";
const GROQ: &str = "/chat/completions";
const UPDATES: &str = "/botfixture-token/getUpdates";
const SEND: &str = "/botfixture-token/sendMessage";

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
}

enum Reply {
    Body {
        status: u16,
        body: Vec<u8>,
        headers: Vec<(&'static str, String)>,
        missing_bytes: usize,
    },
    Disconnect,
}

type Inspect = Box<dyn Fn(&Request) + Send>;

struct Step {
    method: &'static str,
    path: String,
    reply: Reply,
    inspect: Option<Inspect>,
}

fn step(path: impl Into<String>, body: Value) -> Step {
    Step {
        method: "POST",
        path: path.into(),
        reply: Reply::Body {
            status: 200,
            body: serde_json::to_vec(&body).unwrap(),
            headers: vec![],
            missing_bytes: 0,
        },
        inspect: None,
    }
}

impl Step {
    fn get(mut self) -> Self {
        self.method = "GET";
        self
    }
    fn status(mut self, status: u16) -> Self {
        if let Reply::Body { status: code, .. } = &mut self.reply {
            *code = status;
        }
        self
    }
    fn retry_after(mut self, seconds: u64) -> Self {
        if let Reply::Body { headers, .. } = &mut self.reply {
            headers.push(("Retry-After", seconds.to_string()));
        }
        self
    }
    fn inspect(mut self, inspect: impl Fn(&Request) + Send + 'static) -> Self {
        self.inspect = Some(Box::new(inspect));
        self
    }
    fn disconnect(mut self) -> Self {
        self.reply = Reply::Disconnect;
        self
    }
    fn interrupted_body(mut self) -> Self {
        if let Reply::Body { missing_bytes, .. } = &mut self.reply {
            // Deliver headers and body bytes, then close before the declared
            // Content-Length. A complete JSON prefix still has incomplete HTTP
            // framing; this must be a retryable read error, not a JSON error.
            *missing_bytes = 32;
        }
        self
    }
    fn raw_body(mut self, bytes: &[u8]) -> Self {
        if let Reply::Body { body, .. } = &mut self.reply {
            *body = bytes.to_vec();
        }
        self
    }
}

struct Fixture {
    services: Services,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<Vec<Request>>>,
    expected: usize,
}

impl Fixture {
    fn new(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let services = Services::fixture(&format!("http://{}", listener.local_addr().unwrap()));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let expected = steps.len();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let started = Instant::now();
            for expected in steps {
                let mut stream = loop {
                    if stopping.load(Ordering::Relaxed) {
                        return requests;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(30),
                        "HTTP fixture did not receive expected request"
                    );
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                assert_eq!(request.method, expected.method);
                assert_eq!(request.path, expected.path);
                if let Some(inspect) = expected.inspect {
                    inspect(&request);
                }
                if let Reply::Body {
                    status,
                    body,
                    headers,
                    missing_bytes,
                } = expected.reply
                {
                    let mut response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len() + missing_bytes
                    );
                    for (name, value) in headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                    stream.flush().unwrap();
                }
                requests.push(request);
            }
            // Keep the listener alive until the client has returned. This
            // makes an unexpected retry observable instead of letting it fail
            // silently against an already closed fixture port.
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok(_) => panic!("unexpected HTTP request after the final fixture response"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
            requests
        });
        Self {
            services,
            stop,
            server: Some(server),
            expected,
        }
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        let requests = self
            .server
            .take()
            .unwrap()
            .join()
            .expect("HTTP fixture assertions failed");
        assert_eq!(
            requests.len(),
            self.expected,
            "not all expected HTTP requests occurred"
        );
        requests
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap().to_string();
    let path = parts.next().unwrap().to_string();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').expect("valid request header");
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().unwrap();
        }
    }
    assert!(length < 1_000_000);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    }
}

fn update() -> Value {
    json!({"update_id":100,"message":{"message_id":7,"chat":{"id":42},"text":"Please summarize https://youtu.be/jNQXAC9IVRw"}})
}

fn updates() -> Step {
    step(UPDATES, json!({"ok":true,"result":[update()]}))
}
fn empty_updates() -> Step {
    step(UPDATES, json!({"ok":true,"result":[]}))
}
fn transcript(finish: &str) -> Value {
    json!({"candidates":[{"finishReason":finish,"content":{"parts":[
        {"thought":true,"text":"This private reasoning must never reach the summary"},
        {"text":"TITLE: Fixture video\nA private spoken transcript about elephants."}
    ]}}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":20}})
}
fn summary(finish: &str) -> Value {
    json!({"choices":[{"finish_reason":finish,"message":{"content":"TL;DR: Elephants have long trunks.\n• A concrete takeaway."}}],"usage":{"prompt_tokens":40,"completion_tokens":16}})
}
fn accepted() -> Step {
    step(SEND, json!({"ok":true,"result":{"message_id":77}}))
}
fn cfg(directory: &tempfile::TempDir) -> Config {
    Config::fixture(directory.path().join("state.json").to_str().unwrap().into())
}
fn poll(cfg: &Config, fixture: &Fixture, state: &mut State) -> anyhow::Result<usize> {
    app::poll_with_checkpoint(
        cfg,
        &fixture.services,
        &Budget::new(POLL_BUDGET),
        state,
        |state| state.write_local(&cfg.state_path),
    )
}

#[test]
fn real_http_chain_checkpoints_before_send_and_restart_never_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let checkpoint_path = cfg.state_path.clone();
    let fixture = Fixture::new(vec![
        updates().inspect(|request| assert_eq!(request.body["offset"], 0)),
        step(GEMINI, transcript("STOP")).inspect(|request| {
            assert_eq!(
                request.body["contents"][0]["parts"][0]["file_data"]["file_uri"],
                VIDEO
            );
        }),
        step(GROQ, summary("stop")).inspect(|request| {
            assert_eq!(
                request.body["messages"][1]["content"],
                "A private spoken transcript about elephants."
            );
        }),
        accepted().inspect(move |request| {
            assert_eq!(request.body["chat_id"], 42);
            assert!(
                request.body["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("🎬 Fixture video\n\nTL;DR:")
            );
            let persisted = State::load(&checkpoint_path).unwrap();
            assert!(persisted.pending.as_ref().unwrap().awaiting_ack);
            let raw = std::fs::read_to_string(&checkpoint_path).unwrap();
            for private in [
                "elephants",
                "spoken transcript",
                "chat_id",
                "chunks",
                "fixture-token",
            ] {
                assert!(!raw.contains(private));
            }
        }),
        empty_updates().inspect(|request| assert_eq!(request.body["offset"], 101)),
        updates().inspect(|request| assert_eq!(request.body["offset"], 101)),
        empty_updates(),
    ]);
    let mut state = State::default();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
    let mut restored = State::load(&cfg.state_path).unwrap();
    assert_eq!(restored.update_offset, 101);
    assert!(restored.pending.is_none());
    assert_eq!(poll(&cfg, &fixture, &mut restored).unwrap(), 1);
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == SEND)
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == GEMINI)
            .count(),
        1
    );
}

#[test]
fn confirmed_telegram_throttle_is_durable_and_can_retry_without_an_ambiguous_intent() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(), step(GEMINI, transcript("STOP")), step(GROQ, summary("stop")),
        step(SEND, json!({"ok":false,"error_code":429,"parameters":{"retry_after":60},"description":"private response"})).status(429),
        updates(), step(GEMINI, transcript("STOP")), step(GROQ, summary("stop")), accepted(), empty_updates(),
    ]);
    let mut state = State::default();
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(error.is::<telegram::TelegramRejection>());
    let mut state = State::load(&cfg.state_path).unwrap();
    assert!(!state.pending.as_ref().unwrap().awaiting_ack);
    assert!(state.pending.as_ref().unwrap().retry_not_before.unwrap() > unix_time());
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    // Advance the fixture's persisted clock boundary without a real minute wait.
    state.pending.as_mut().unwrap().retry_not_before = Some(unix_time().saturating_sub(1));
    state.write_local(&cfg.state_path).unwrap();
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 1);
    assert_eq!(state.update_offset, 101);
    fixture.finish();
}

#[test]
fn confirmed_forbidden_chat_does_not_poison_the_following_poll() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")),
        step(
            SEND,
            json!({"ok":false,"error_code":403,"description":"private response"}),
        )
        .status(403),
        empty_updates().inspect(|request| assert_eq!(request.body["offset"], 101)),
    ]);
    let mut state = State::default();
    assert!(poll(&cfg, &fixture, &mut state).is_err());
    let mut state = State::load(&cfg.state_path).unwrap();
    assert!(state.pending.is_none());
    assert_eq!(state.update_offset, 101);
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    fixture.finish();
}

#[test]
fn lost_telegram_ack_keeps_the_durable_intent_and_blocks_a_duplicate_on_restart() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")),
        accepted().disconnect(),
    ]);
    let mut state = State::default();
    assert!(poll(&cfg, &fixture, &mut state).is_err());
    let mut state = State::load(&cfg.state_path).unwrap();
    assert!(state.pending.as_ref().unwrap().awaiting_ack);
    assert_eq!(state.update_offset, 0);
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(error.to_string().contains("reconciliation"));
    assert_eq!(
        fixture
            .finish()
            .iter()
            .filter(|request| request.path == SEND)
            .count(),
        1
    );
}

#[test]
fn provider_retries_cover_real_http_503_and_429_and_process_needs_no_telegram() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = cfg(&directory);
    cfg.telegram_token = None;
    let fixture = Fixture::new(vec![
        step(
            GEMINI,
            json!({"error":"private response must not be logged"}),
        )
        .status(503)
        .retry_after(0),
        step(GEMINI, transcript("STOP")),
        step(GROQ, json!({"error":"private response"}))
            .status(429)
            .retry_after(0),
        step(GROQ, summary("stop")),
    ]);
    let output =
        app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap();
    assert!(output.contains("TL;DR:"));
    assert!(!output.contains("private reasoning"));
    fixture.finish();
}

#[test]
fn provider_retry_limit_is_finite_and_error_identifies_the_stage_without_body() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(
        (0..3)
            .map(|_| {
                step(GEMINI, json!({"secret":"private provider body"}))
                    .status(503)
                    .retry_after(0)
            })
            .collect(),
    );
    let error =
        app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("gemini") && rendered.contains("transcribe") && rendered.contains("503")
    );
    assert!(!rendered.contains("private") && !rendered.contains("fixture-gemini-key"));
    fixture.finish();
}

#[test]
fn interrupted_provider_bodies_retry_gemini_and_groq_through_process_video() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = cfg(&directory);
    cfg.telegram_token = None;
    let fixture = Fixture::new(vec![
        step(GEMINI, transcript("STOP")).interrupted_body(),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")).interrupted_body(),
        step(GROQ, summary("stop")),
    ]);
    let output =
        app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap();
    assert!(output.contains("TL;DR: Elephants have long trunks."));
    assert!(!output.contains("private reasoning"));
    let requests = fixture.finish();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].body == requests[1].body);
    assert!(requests[2].body == requests[3].body);
}

#[test]
fn interrupted_provider_bodies_exhaust_bounded_retries_without_terminal_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        step(GEMINI, transcript("STOP")).interrupted_body(),
        step(GEMINI, transcript("STOP")).interrupted_body(),
        step(GEMINI, transcript("STOP")).interrupted_body(),
    ]);
    let mut state = State::default();
    let error = poll(&cfg, &fixture, &mut state).unwrap_err();
    assert!(error.to_string().contains("temporary provider failure"));
    let state = State::load(&cfg.state_path).unwrap();
    assert_eq!(state.update_offset, 0);
    assert!(!state.is_processed("42:7"));
    let pending = state.pending.as_ref().unwrap();
    assert_eq!(pending.update_id, 100);
    assert_eq!(pending.attempts, 1);
    assert!(!pending.awaiting_ack);
    assert!(pending.chunks.is_none());
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == GEMINI)
            .count(),
        3
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.path == GROQ || request.path == SEND)
    );
}

#[test]
fn complete_malformed_provider_json_is_safe_and_nonretryable_after_one_request() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture =
        Fixture::new(vec![step(GEMINI, Value::Null).raw_body(
            br#"{"private-response-fixture":"fixture-gemini-key","candidates":["#,
        )]);
    let error =
        app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap_err();
    assert!(!error.downcast_ref::<ProviderError>().unwrap().retryable());
    let rendered = format!("{error:#}");
    assert!(rendered.contains("gemini") && rendered.contains("malformed JSON"));
    for private in [
        "private-response-fixture",
        "fixture-gemini-key",
        "candidates",
    ] {
        assert!(!rendered.contains(private));
    }
    assert_eq!(fixture.finish().len(), 1);
}

#[test]
fn provider_long_retry_after_survives_restart_without_spending_another_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        updates(),
        step(GEMINI, json!({"error":"busy"}))
            .status(503)
            .retry_after(3600),
    ]);
    let mut state = State::default();
    assert!(poll(&cfg, &fixture, &mut state).is_err());
    let mut state = State::load(&cfg.state_path).unwrap();
    assert!(state.pending.as_ref().unwrap().retry_not_before.unwrap() >= unix_time() + 3599);
    assert_eq!(state.pending.as_ref().unwrap().attempts, 1);
    assert_eq!(poll(&cfg, &fixture, &mut state).unwrap(), 0);
    assert_eq!(state.pending.as_ref().unwrap().attempts, 1);
    fixture.finish();
}

#[test]
fn transcription_contract_rejects_blocked_empty_or_malformed_responses() {
    for response in [
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
        json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":""}]}}]}),
        json!({"candidates":[{"finishReason":"SAFETY","content":{"parts":[{"text":"partial"}]}}]}),
        json!({"unexpected":"shape"}),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = cfg(&directory);
        let fixture = Fixture::new(vec![step(GEMINI, response)]);
        let error = app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO)
            .unwrap_err();
        assert!(error.to_string().contains("gemini"));
        fixture.finish();
    }
}

#[test]
fn no_speech_never_calls_the_summarizer() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![step(
        GEMINI,
        json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"NO_SPEECH"}]}}]}),
    )]);
    let output =
        app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap();
    assert!(output.contains("Не нашёл"));
    fixture.finish();
}

#[test]
fn either_provider_truncation_always_reaches_the_user_as_a_warning() {
    for (transcript_finish, summary_finish) in [("MAX_TOKENS", "stop"), ("STOP", "length")] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = cfg(&directory);
        let fixture = Fixture::new(vec![
            step(GEMINI, transcript(transcript_finish)),
            step(GROQ, summary(summary_finish)),
        ]);
        let output =
            app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap();
        assert!(output.ends_with(app::TRUNCATION_WARNING));
        fixture.finish();
    }
}

#[test]
fn gemini_three_uses_supported_thinking_level() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = cfg(&directory);
    cfg.gemini_model = "gemini-3.8-flash".into();
    let fixture = Fixture::new(vec![
        step(
            "/models/gemini-3.8-flash:generateContent",
            transcript("STOP"),
        )
        .inspect(|request| {
            assert_eq!(
                request.body["generationConfig"]["thinkingConfig"],
                json!({"thinkingLevel":"LOW"})
            );
        }),
        step(GROQ, summary("stop")),
    ]);
    app::process_video(&cfg, &fixture.services, &Budget::new(POLL_BUDGET), VIDEO).unwrap();
    fixture.finish();
}

#[test]
fn doctor_only_reads_bot_webhook_and_model_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        step("/botfixture-token/getMe", json!({"ok":true,"result":{"is_bot":true,"username":"fixture_bot"}})),
        step("/botfixture-token/getWebhookInfo", json!({"ok":true,"result":{"url":""}})),
        step("/models/gemini-fixture", json!({"name":"models/gemini-fixture","supportedGenerationMethods":["generateContent"]})).get(),
        step("/groq/models", json!({"object":"list","data":[
            {"id":"fixture/summary-other","active":false},
            {"id":"fixture/summary","active":true}
        ]})).get(),
    ]);
    let report = app::doctor(&cfg, &fixture.services, &Budget::new(POLL_BUDGET)).unwrap();
    assert!(report.contains("https://t.me/fixture_bot") && report.contains("Allowed chats: 1"));
    for private in [
        "42",
        "fixture-token",
        "fixture-gemini-key",
        "fixture-groq-key",
    ] {
        assert!(!report.contains(private));
    }
    assert!(!std::path::Path::new(&cfg.state_path).exists());
    fixture.finish();
}

#[test]
fn doctor_model_readiness_rejects_missing_inactive_or_malformed_groq_entries() {
    for (case, response) in [
        (
            "missing configured id",
            json!({"object":"list","data":[{"id":"fixture/summary-other","active":true}]}),
        ),
        (
            "inactive configured id",
            json!({"object":"list","data":[
                {"id":"fixture/summary-other","active":true},
                {"id":"fixture/summary","active":false}
            ]}),
        ),
        (
            "malformed data",
            json!({"object":"list","data":{"id":"fixture/summary","active":true}}),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let cfg = cfg(&directory);
        let fixture = Fixture::new(vec![
            step(
                "/models/gemini-fixture",
                json!({"name":"models/gemini-fixture","supportedGenerationMethods":["generateContent"]}),
            )
            .get(),
            step("/groq/models", response).get(),
        ]);
        let error = crate::http::check_models(&cfg, &fixture.services, &Budget::new(POLL_BUDGET))
            .expect_err(case);
        assert!(!error.downcast_ref::<ProviderError>().unwrap().retryable());
        let rendered = format!("{error:#}");
        assert!(rendered.contains("groq") && rendered.contains("model_readiness"));
        assert!(!rendered.contains("fixture-groq-key") && !rendered.contains("fixture/summary"));
        assert_eq!(fixture.finish().len(), 2);
    }
}

#[test]
fn doctor_rejects_an_active_webhook_without_mutating_it_or_printing_its_url() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![
        step(
            "/botfixture-token/getMe",
            json!({"ok":true,"result":{"is_bot":true,"username":"fixture_bot"}}),
        ),
        step(
            "/botfixture-token/getWebhookInfo",
            json!({"ok":true,"result":{"url":"https://private.invalid/secret"}}),
        ),
    ]);
    let error = app::doctor(&cfg, &fixture.services, &Budget::new(POLL_BUDGET)).unwrap_err();
    assert!(error.to_string().contains("webhook"));
    assert!(!format!("{error:#}").contains("private.invalid"));
    fixture.finish();
}

#[test]
fn setup_requires_only_the_telegram_token() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = cfg(&directory);
    cfg.gemini_key = None;
    cfg.groq_key = None;
    let fixture = Fixture::new(vec![step(
        "/botfixture-token/setMyCommands",
        json!({"ok":true,"result":true}),
    )]);
    telegram::setup_commands(&cfg, &fixture.services, &Budget::new(POLL_BUDGET)).unwrap();
    fixture.finish();
}

#[test]
fn exhausted_poll_budget_does_not_fetch_or_acknowledge_updates() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let fixture = Fixture::new(vec![]);
    let mut state = State::default();
    assert_eq!(
        app::poll_with_checkpoint(
            &cfg,
            &fixture.services,
            &Budget::new(Duration::from_secs(30)),
            &mut state,
            |_| panic!("no checkpoint without work")
        )
        .unwrap(),
        0
    );
    assert_eq!(state.update_offset, 0);
    fixture.finish();
}

#[test]
fn provider_budget_exhaustion_retains_the_update_without_a_terminal_failure_reply() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = cfg(&directory);
    let expired = Fixture::new(vec![]);
    let update: telegram::Update = serde_json::from_value(update()).unwrap();
    let mut state = State::default();
    let error = crate::runner::process_update(
        &mut state,
        &update,
        true,
        |_| {
            app::process_video(&cfg, &expired.services, &Budget::new(Duration::ZERO), VIDEO)
                .map(Some)
        },
        |_, _| panic!("budget exhaustion must not send a terminal failure reply"),
        |state| state.write_local(&cfg.state_path),
    )
    .unwrap_err();
    assert!(error.is::<crate::runtime::BudgetExhausted>());
    let mut state = State::load(&cfg.state_path).unwrap();
    assert_eq!(state.update_offset, 0);
    assert!(state.pending.is_some());
    assert!(!state.pending.as_ref().unwrap().awaiting_ack);
    expired.finish();
    let retry = Fixture::new(vec![
        updates(),
        step(GEMINI, transcript("STOP")),
        step(GROQ, summary("stop")),
        accepted(),
        empty_updates(),
    ]);
    assert_eq!(poll(&cfg, &retry, &mut state).unwrap(), 1);
    assert_eq!(state.update_offset, 101);
    retry.finish();
}
