# yt-tldr

Telegram-бот: кидаешь ссылку на YouTube — получаешь саммари с ключевыми тейками.
Полностью serverless: никакого сервера, вся логика крутится в GitHub Actions.

```
ты → Telegram
        │  cron */5: «Run Bot»
        ▼
   Rust (qqrm-labs/yt-tldr)
        │  1. getUpdates → вытащить YouTube-ссылку
        │  2. Gemini (free tier) → полный транскрипт видео по ссылке
        │  3. Groq (free tier)   → TL;DR + тейки
        ▼
ты ← sendMessage ← ┘

состояние (update offset + дедуп) → ветка bot-state
```

## Почему так

- **Транскрипция — Gemini, не Яндекс.** Yandex SpeechKit требует биллинг-аккаунт с картой,
  а Gemini бесплатного тира принимает ссылку на YouTube **напрямую** и сам отдаёт транскрипт —
  это и есть «гугловая транскрипция», причём без скачивания видео: yt-dlp с IP раннеров
  Actions YouTube регулярно блокирует.
- **Саммари — Groq** (`openai/gpt-oss-120b` по умолчанию): free tier, паттерн проверен на
  [weekly-gh-digest](../weekly-gh-digest).
- **Репо публичное** намеренно: cron каждые 5 минут на приватном репо сжёг бы лимит 2000
  Actions-минут в месяц; на публичном минуты не ограничены. Секреты в GitHub Secrets не
  утекают, логи маскируются.

## Лимиты бесплатных тиров

- Gemini: дневные лимиты на запросы и на суммарную длину видео (актуальные числа — в
  docs AI Studio). Для личного бота хватает с запасом; длинные видео (>1 часа) могут
  обрезать транскрипт по лимиту вывода — об этом будет пометка в саммари.
- Groq: дневной лимит запросов на модель.
- Telegram: 30 сообщений/сек — не наш случай.

## Setup (один раз)

1. [@BotFather](https://t.me/BotFather) → `/newbot` → получить токен.
2. [aistudio.google.com/apikey](https://aistudio.google.com/apikey) → ключ Gemini (без карты).
3. Ключ Groq (console.groq.com) — можно тот же, что использует weekly-gh-digest.
4. Залить секреты:
   ```sh
   gh secret set -R qqrm-labs/yt-tldr TELEGRAM_BOT_TOKEN
   gh secret set -R qqrm-labs/yt-tldr GEMINI_API_KEY
   gh secret set -R qqrm-labs/yt-tldr GROQ_API_KEY
   ```
   Модели (опционально) — в repo variables `GEMINI_MODEL` / `GROQ_MODEL`.
5. `gh workflow run -R qqrm-labs/yt-tldr "Run Bot"` — первый прогон (или дождаться cron).
6. Кинуть боту ссылку на видео, подождать до ~10 минут (cron + обработка).

Обязательно для polling: `ALLOWED_CHAT_IDS` (secret, comma-separated) — явный белый список чатов. Без него бот отказывается обращаться к провайдерам.

## Локально

```sh
export TELEGRAM_BOT_TOKEN=... GEMINI_API_KEY=... GROQ_API_KEY=...
cargo run -- process 'https://youtu.be/...'   # без Telegram, саммари в stdout
cargo run -- poll                             # один цикл опроса Telegram
cargo run -- setup                            # setMyCommands
```

## Файлы

- `src/` — Rust: `main.rs` (poll/process), `gemini.rs` (транскрипт), `groq.rs` (саммари),
  `telegram.rs` (Bot API), `state.rs`, `youtube.rs`.
- `.github/workflows/run-bot.yml` — прод-пайплайн (cron + ручной тест с `youtube_url`).
- `.github/workflows/ci.yml`, `workflow-security.yml` — fmt/clippy/test, actionlint/zizmor.
- Ветка `bot-state` — персистентное состояние (паттерн weekly-gh-digest).

## Идеи на потом

- Webhook через Cloudflare Worker (бесплатно) → мгновенная реакция вместо cron.
- Long-poll режим: один job на несколько часов → ответ за секунды.
- Выбор языка саммари командой бота.

## Checkpoint and maintenance guarantees

State files are atomically replaced. Only a confirmed missing file/remote state branch permits bootstrap; unreadable, corrupt, or unreachable state stops the run. The save step only follows a successful restore and refuses to overwrite a state branch that changed during the run. Delivery checkpoints contain metadata only: update ID, retry count, chunk cursor and SHA-256 fingerprints. Message/chat contents and replies are never written to the public state branch. Legacy dedup keys are migrated to hashes on load; existing branch history is not rewritten.

Telegram has no idempotency key: a crash after Telegram accepts a message but before its checkpoint is persisted can still duplicate that message. GitHub runner loss before the final state-branch push can also lose local checkpoints. Exactly-once delivery is not claimed.

Dependabot batches weekly maintenance updates. CI and workflow-security gates require every constituent job to succeed; verified bot commits are matched to the exact checked head before native auto-merge.

Polling requires the `ALLOWED_CHAT_IDS` secret (comma-separated integer chat IDs); malformed or missing allowlists stop polling before paid providers are called. `process <url>` remains independent of Telegram credentials. Transient provider HTTP 408/429/5xx and connection/timeouts retain the update for up to three persisted attempts; failed or exhausted requests produce a generic safe reply. Prepared replies live only in memory. After restart, Telegram must still retain the original update; providers may be called again within the same persisted three-attempt budget. A partial reply resumes only if the regenerated chunk list has the same SHA-256 fingerprint. A mismatch or expired update stops processing for operator reconciliation; processed/offset markers advance only after completion. A Telegram send error retains the unsent tail for a later run.

For a reconciliation error, inspect the already received Telegram messages before changing pending metadata. Decide whether to abandon the remainder or deliberately resend the full reply; there is no automatic reset that can silently duplicate messages. Durable private reply storage is a separate architectural decision.
