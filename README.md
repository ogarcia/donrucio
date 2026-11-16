# donrucio

A tiny Telegram bot that answers questions about **Rucio** from its own
documentation, using a free [Groq](https://groq.com) model.

- **Lightweight:** one small Rust binary, a blocking HTTP client (`ureq`), no
  async runtime, no framework.
- **No open ports:** talks to Telegram over long polling, so it runs behind any
  NAT — a home machine is enough.
- **Always up to date:** it pulls the docs straight from GitHub at startup
  (and periodically), caching them locally as a fallback. Update the docs in the
  repo and the bot follows, no rebuild.
- **Grounded:** it answers only from the docs and says so when it does not know,
  pointing to the project instead of inventing commands.

## How it works

1. On startup it lists `docs/user/*.md` (+ `README.md`) in the Rucio repo via the
   GitHub API, downloads them, and caches the result.
2. For each question it retrieves only the most relevant doc sections (keyword
   match with a small Spanish→English synonym map) and sends those as context —
   Groq's free tier caps a request at 8000 tokens, so the whole manual cannot be
   sent at once.
3. It asks the model and replies on Telegram.

## Setup

1. **Create a bot** with [@BotFather](https://t.me/BotFather) → get the token.
2. **Get a free Groq key** at <https://console.groq.com/keys> (no card).
3. Configure:
   ```sh
   cp .env.example .env
   # edit .env: TELEGRAM_BOT_TOKEN and GROQ_API_KEY
   ```
4. **Build and run:**
   ```sh
   cargo build --release
   ./target/release/donrucio
   ```

Secrets come from the environment (a `.env` file is loaded automatically).
Never commit `.env`.

### Test without Telegram

Ask a single question on the command line (only needs `GROQ_API_KEY`):

```sh
GROQ_API_KEY=gsk_... ./target/release/donrucio --ask "¿Cómo limito la subida?"
```

## Configuration

Everything is set through environment variables — see
[`.env.example`](.env.example). The model (`GROQ_MODEL`), the docs source
(`DOCS_REPO` / `DOCS_PATH` / `DOCS_REF`) and the retrieval budget
(`CONTEXT_CHARS`, `MAX_TOKENS`) are all overridable.

List the models your key can use:

```sh
curl -s https://api.groq.com/openai/v1/models \
  -H "Authorization: Bearer $GROQ_API_KEY" | python3 -m json.tool
```

## Using it in a group

Add the bot to a group and it answers only when addressed — it never butts into
normal conversation. There are two ways to reach it, depending on the bot's
Telegram **privacy mode** (BotFather → `/setprivacy`):

- **Privacy off** (remove and re-add the bot after changing it): people can just
  **@mention** it (`@YourBot ¿cómo instalo Rucio?`) or reply to one of its
  messages. The bot receives every group message but only responds when mentioned
  or replied to.
- **Privacy on** (default, more discreet — the bot only ever receives what is
  aimed at it): plain @mentions are not delivered by Telegram, so use the
  **command** `/rucio <pregunta>` (aliases: `/ask`, `/pregunta`). Commands are
  always delivered.

Either way `/rucio <pregunta>` works, so it is the safe choice if you are unsure.

## Collecting suggestions

The bot also recognises **feature suggestions**, not just questions. For each it
makes a judgement:

- Something you can already do or configure yourself (it is in the docs, or it is
  your own setup — e.g. adding a package to your container image): it just tells
  you how, and records nothing.
- A genuine idea that is not possible yet (a missing feature, a UX improvement):
  it acknowledges it — **without promising anything** — and appends a one-line
  summary to a log for the maintainer to review.

Suggestions are written as JSON Lines to `SUGGESTIONS_FILE` (one object per line
with the summary, the original message, a timestamp and the chat/user it came
from). It defaults to `suggestions.jsonl` **beside the docs cache**, so the same
volume or `StateDirectory` that persists the cache keeps the suggestions too. Set
`SUGGESTIONS_FILE=` (empty) to disable logging. Review them with, e.g.:

```sh
jq -r '.date + "  " + .summary' suggestions.jsonl
```

## Run as a service (systemd, native binary)

Install the binary (e.g. to `/usr/local/bin/donrucio`) and create
`/etc/systemd/system/donrucio.service` with the tokens set directly as
environment variables:

```ini
[Unit]
Description=Don Rucio — Telegram help bot
After=network-online.target
Wants=network-online.target

[Service]
Environment=TELEGRAM_BOT_TOKEN=123456789:your-botfather-token
Environment=GROQ_API_KEY=gsk_your_groq_key
# Optional overrides, one Environment= line each:
#Environment=GROQ_MODEL=openai/gpt-oss-120b
#Environment=HISTORY_IDLE_MINS=30
ExecStart=/usr/local/bin/donrucio
Restart=on-failure
RestartSec=5
DynamicUser=yes
StateDirectory=donrucio
Environment=DOCS_CACHE=%S/donrucio/docs.md

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now donrucio
journalctl -u donrucio -f          # follow the logs
```

The tokens live in the unit, which is root-owned under `/etc/systemd/system`.
If you prefer to keep them out of the unit, replace the `Environment=` token
lines with `EnvironmentFile=/etc/donrucio.env` (a `chmod 600` file).

## Run with Podman / Docker

Pass the tokens as environment variables — no `.env` needed. The bot talks to
Telegram over long polling (outbound only), so it needs **no open ports** and
runs behind any NAT:

```sh
podman run -d --name donrucio --restart=unless-stopped \
  -e TELEGRAM_BOT_TOKEN=123456789:your-botfather-token \
  -e GROQ_API_KEY=gsk_your_groq_key \
  ghcr.io/ogarcia/donrucio:latest
```

(Use `docker` in place of `podman`; the flags are identical.) Any override is
just another `-e`, e.g. `-e GROQ_MODEL=openai/gpt-oss-20b`. To keep the docs
cache across restarts, mount a volume and point the cache at it:

```sh
  -e DOCS_CACHE=/data/docs.md -v donrucio-data:/data
```

Logs: `podman logs -f donrucio`.

## Notes and limits

- Groq's free tier is **8000 tokens/minute** and ~1000 requests/day per model —
  plenty for a low-traffic help bot. A burst of questions in the same minute may
  hit the rate limit; the bot then asks the user to retry.
- `gpt-oss` models reason before answering; `REASONING_EFFORT=low` keeps that
  cheap.
- Retrieval is keyword-based, so very indirect phrasing may miss the right
  section. Extend the synonym map in `src/main.rs` if you spot gaps.
