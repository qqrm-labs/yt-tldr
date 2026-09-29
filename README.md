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

Опционально: `ALLOWED_CHAT_IDS` (secret, comma-separated) — белый список чатов, если
ник бота кому-то стал известен и чужаки начали жечь квоту.

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
