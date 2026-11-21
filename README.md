# yt-tldr

Telegram-бот: кидаешь ссылку на YouTube — получаешь саммари с ключевыми тейками.
Логика выполняется в GitHub Actions: Telegram → YouTube → транскрипция Gemini →
саммари Groq → ответ в Telegram. Отдельный сервер не нужен.

Это асинхронный бот: `schedule` запрашивает запуск каждые пять минут, но GitHub может
задерживать или пропускать такие запуски. Обещания ответа за 0–10 минут нет.
Для обработки очереди сейчас доступен ручной `Run Bot` с `operation=poll`.
Постоянно доступному боту с быстрым ответом нужен отдельный runtime или webhook.

Авторитетное состояние доставки хранится в `delivery-state`; `bot-state` содержит архив.

## Почему так

- **Транскрипция — Gemini, не Яндекс.** Yandex SpeechKit требует биллинг-аккаунт с картой,
  а Gemini бесплатного тира принимает ссылку на YouTube **напрямую** и сам отдаёт транскрипт —
  это и есть «гугловая транскрипция», причём без скачивания видео: yt-dlp с IP раннеров
  Actions YouTube регулярно блокирует.
- **Саммари — Groq** (`openai/gpt-oss-120b` по умолчанию): free tier, паттерн проверен на
  [weekly-gh-digest](../weekly-gh-digest).
- **Репо публичное:** ключи хранятся только в GitHub Actions secrets. Состояние доставки
  содержит метаданные, а не пользовательский текст, chat IDs или сгенерированные ответы.
  Ручная проверка видео выводит саммари в публичный summary запуска: для неё
  используйте только публичный тестовый материал.

## Лимиты бесплатных тиров

- Доступность моделей и бесплатные квоты зависят от аккаунта провайдера. Наличие ключа
  не доказывает, что обработка конкретного видео доступна: это проверяется отдельным
  запуском `process`.
- Длинные видео могут не поместиться в лимиты транскрипции или саммари. Обрезание
  отмечается в итоговом ответе; полный дословный транскрипт не гарантируется.
- Временные ошибки провайдеров повторяются ограниченное число раз. Ошибка настройки,
  недоступная модель или исчерпание попыток видны в журнале с названием этапа.

## Setup (один раз)

1. [@BotFather](https://t.me/BotFather) → `/newbot` → получить токен.
2. [aistudio.google.com/apikey](https://aistudio.google.com/apikey) → ключ Gemini (без карты).
3. Ключ Groq (console.groq.com) — можно тот же, что использует weekly-gh-digest.
4. Залить секреты:
   ```sh
   gh secret set -R qqrm/yt-tldr TELEGRAM_BOT_TOKEN
   gh secret set -R qqrm/yt-tldr GEMINI_API_KEY
   gh secret set -R qqrm/yt-tldr GROQ_API_KEY
   gh secret set -R qqrm/yt-tldr ALLOWED_CHAT_IDS
   ```
   `ALLOWED_CHAT_IDS` — явный список разрешённых chat IDs через запятую, без `*`.
   Модели (опционально) — в repo variables `GEMINI_MODEL` / `GROQ_MODEL`.
5. Проверить настройки, токен и отсутствие конфликтующего webhook:
   ```sh
   gh workflow run -R qqrm/yt-tldr "Run Bot" -f operation=check
   ```
   Summary покажет имя и ссылку бота, а также число разрешённых чатов. Проверка не
   забирает входящие сообщения и не отправляет ответов.
6. Настроить меню `/start` и `/help`:
   ```sh
   gh workflow run -R qqrm/yt-tldr "Run Bot" -f operation=setup
   ```
7. В разрешённом личном чате открыть ссылку бота, нажать **Start** и отправить YouTube URL.
   Дождаться следующего запуска или обработать очередь явно:
   ```sh
   gh workflow run -R qqrm/yt-tldr "Run Bot" -f operation=poll
   ```

Ручной запуск без параметров выполняет `check`. Плановый запуск выполняет `poll`.
Polling требует Telegram token, allowlist и оба ключа провайдеров; неполная настройка
завершает запуск ошибкой, а не зелёным пропуском.

## Как проверить обработку видео

Один короткий публичный пример, без Telegram и без изменения веток состояния:

```sh
gh workflow run -R qqrm/yt-tldr "Run Bot" \
  -f youtube_url='https://www.youtube.com/watch?v=jNQXAC9IVRw'
```

Этот режим реально вызывает Gemini и Groq и публикует результат в summary запуска.
Он подтверждает только обработку видео. Полный сценарий подтверждается отдельным
входящим сообщением в Telegram, успешным `sendMessage` с receipt и следующим polling
без повторной отправки. Зелёный polling с пустой очередью этого не доказывает.

## Локально

```sh
export TELEGRAM_BOT_TOKEN=... GEMINI_API_KEY=... GROQ_API_KEY=...
cargo run -- process 'https://youtu.be/...'   # без Telegram, саммари в stdout
export ALLOWED_CHAT_IDS=...
cargo run -- doctor                          # проверка готовности без сообщений
cargo run -- poll                             # один цикл опроса Telegram
cargo run -- setup                            # setMyCommands
```

## Файлы

- `src/` — Rust: `main.rs` (poll/process), `gemini.rs` (транскрипт), `groq.rs` (саммари),
  `telegram.rs` (Bot API), `state.rs`, `youtube.rs`.
- `.github/workflows/run-bot.yml` — прод-пайплайн (cron + ручной тест с `youtube_url`).
- `.github/workflows/ci.yml`, `workflow-security.yml` — fmt/clippy/test, actionlint/zizmor.
- Ветка `delivery-state` — авторитетный checkpoint; `bot-state` — архив.

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

A structured Telegram rejection is distinct from an unknown delivery outcome. A confirmed
429 retains the update with its retry time; a confirmed terminal 400/403 is reported as a
delivery failure without blocking unrelated updates. A missing or malformed acknowledgement
still requires reconciliation. Partial replies are regenerated after a restart and must match
the stored fingerprint; reply text is deliberately not persisted in this public repository.

To reconcile, first stop concurrent runs and save the current checkpoint and its SHA. Inspect
the destination chat privately. For a proven absent chunk, clear only `pending.awaiting_ack`;
for a proven delivered chunk, advance `pending.next_chunk` by one and clear that flag. Keep
all other fields and write with the inspected blob SHA. If delivery cannot be established,
leave the intent pending. Never clear the checkpoint or reset the update offset to recover.
An unavailable original Telegram update or changed regenerated reply still requires manual
reconciliation. Local runs without the remote helper retain local checkpoints only.

The production workflow only runs from `main` and uses a repository-wide concurrency group.
Automated fixture tests use local HTTP fixtures and require no production credentials.
Manual video checks use the configured provider keys, require no Telegram token, and never
send Telegram messages. `doctor` reads service metadata; `setup` configures the bot menu.

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
