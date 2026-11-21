# Repository Agent Instructions

## What this is
Rust Telegram bot: a YouTube link in, progress and a summary with key takeaways out.
Production uses one persistent `serve` process with long polling and private local
state. GitHub Actions runs CI, readiness, menu setup and public video diagnostics;
it must not be turned into a self-restarting permanent bot host.

## Pipeline and identity
getUpdates → accepted status → Gemini transcription → Groq summary → final reply.
Progress edits are best effort; confirmed result delivery has a separate durable
intent and cursor. Never acknowledge Telegram input before delivery is resolved.
The dedicated token is `YT_TELEGRAM_BOT_TOKEN` and the expected username is
`EXPECTED_TELEGRAM_BOT_USERNAME`. Do not fall back to the digest bot's old token.
State is bound to bot identity. Never copy offsets across bots or reset state to
recover from ambiguous delivery. Historical `delivery-state` and `bot-state`
branches belong to the retired Actions worker and remain available for recovery.

## Validation loop
- `cargo fmt --all -- --check`
- `cargo check --locked --tests`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo test --locked`

## Deployment and workflow rules
- Never bypass failing checks on `main`.
- Keep credentials in GitHub Actions secrets or the deployment host's private
  environment file. Never log, commit or extract secret values.
- Model names and expected bot username are non-secret configuration. In Actions
  they belong in repository variables.
- Public state/logs must not contain chat IDs, input text, video URLs, transcripts,
  generated replies or tokens. Fixture data is synthetic.
- Actions are pinned by commit SHA with a version comment; retain that convention.
- `process <url>` must never require a Telegram token.
- Automated tests use local fixtures; they never send Telegram messages.
- A real deployment is verified by receipt, progress, result and restart without
  duplicate delivery. Green CI or an empty queue does not establish that result.
