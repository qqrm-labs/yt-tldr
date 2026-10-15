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

## Durable delivery and recovery

The workflow restores the existing `bot-state` source archive, then overlays the authoritative
`delivery-state/yt-tldr.json` checkpoint. On first use, it migrates the existing state; an
existing archive without its expected state file is an error. Every provider reservation,
pre-send intent, and acknowledged cursor is committed synchronously with the GitHub Contents
API using compare-and-swap. A stale writer, transport error, or missing restore receipt stops
processing. The final archive push is no longer the durability boundary.

The public checkpoint contains only update IDs, attempt counts, delivery cursors, fingerprints,
and `awaiting_ack`; user text, chat IDs, generated replies, and tokens are never serialized.
If Telegram times out or the runner dies after the intent was stored, the next run stops with
`operator reconciliation required` instead of automatically sending a possible duplicate.
Telegram does not support an idempotency key, so exactly-once delivery is not claimed.

To reconcile, first stop concurrent runs and save the current checkpoint and its SHA. Inspect
the destination chat privately. For a proven absent chunk, clear only `pending.awaiting_ack`;
for a proven delivered chunk, advance `pending.next_chunk` by one and clear that flag. Keep
all other fields and write with the inspected blob SHA. If delivery cannot be established,
leave the intent pending. Never clear the checkpoint or reset the update offset to recover.
An unavailable original Telegram update or changed regenerated reply still requires manual
reconciliation. Local runs without the remote helper retain local checkpoints only.

The production workflow only runs from `main` and uses a repository-wide concurrency group.
No test command sends Telegram messages or requires production credentials.

The checkout never persists its token. Legacy archive fetch/push receives `GH_TOKEN` only in
its own step and uses a process-scoped Git credential helper; no authentication is written
to the Git configuration. Dependency builds and caches receive no publication credential.
The checkpoint helper and bot receive their required credentials only during their runtime steps.

The checkpoint uses the application's typed JSON state, preserving existing fields and adding
defaults only for compatible fields. This migration creates its
single known state key only when `delivery-state` is confirmed absent. A pre-existing branch
without that key is not an empty installation: it stops for explicit initialization or
reconciliation. Adding another key later requires an intentional migration, never automatic
fallback to an empty state. The `bot-state` archive is the initial migration source only;
subsequent runs always restore the authoritative checkpoint over that archive.

After migration, use the current workflow from `main`; do not rerun historical jobs from before
the checkpoint migration, because their old code does not implement this delivery contract.
