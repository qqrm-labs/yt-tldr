# Repository Agent Instructions

## What this is
Telegram bot: a YouTube link in, a summary with key takeaways out. Fully serverless —
logic runs in GitHub Actions ("Run Bot", cron */5), persistent state lives on the
`bot-state` branch (`data/bot-state.json`), same pattern as weekly-gh-digest.

## Pipeline
getUpdates drain → extract YouTube URL → Gemini free tier (native YouTube URL
understanding, no video download) → transcript → Groq (`openai/gpt-oss-120b`) →
TL;DR + takeaways → sendMessage. The binary synchronously checkpoints metadata to `delivery-state` through the workflow-provided
helper. The workflow retains `bot-state` as an archive; it is not the send durability boundary.

## Validation loop
- `cargo fmt --all -- --check`
- `cargo check --tests`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

## Workflow rules
- Never bypass failing checks on `main`.
- Keep secrets only in GitHub Actions secrets (TELEGRAM_BOT_TOKEN, GEMINI_API_KEY,
  GROQ_API_KEY); model names go to repo variables (GEMINI_MODEL, GROQ_MODEL).
- Actions are pinned by commit SHA with a version comment — keep it that way
  (workflow-security.yml runs actionlint + zizmor on every change).
- The `process <url>` subcommand is the manual test path: it must never need
  TELEGRAM_BOT_TOKEN.
