# frank-opencode

Local Anthropic-compatible gateway that exposes your **OpenCode models** (v2) to **Claude Code**.

Runs on `127.0.0.1` only, reuses the OpenCode login already on the machine, and translates
Anthropic Messages ↔ upstream (Anthropic passthrough or OpenAI Chat Completions).

```bash
frank-opencode --enable    # start in background
frank-opencode --status
frank-opencode --disable   # stop
```

## How it works

| Concern | Implementation |
|---|---|
| Credentials (OpenCode v2) | Read-only from SQLite `opencode.db`, table `credential` (`{"type","key"}` envelope unwrapped in memory, never logged). Path via `opencode debug paths db`, else `OPENCODE_DB` / `XDG_DATA_HOME`. `auth.json` is legacy migration input only. |
| Model catalog | `opencode api get /api/model` (service-auth aware), `enabled` only. |
| Anthropic-package models (MiniMax, Qwen…) | Byte passthrough to `{baseURL}/messages` (`anthropic-version`/`anthropic-beta` forwarded). |
| OpenAI-compatible models (Kimi, GLM, DeepSeek…) | Translated to `{baseURL}/chat/completions` and back, incl. `tool_use`/`tool_result`, images, streaming SSE. |
| Responses-API models (Go GPT/Grok/Muse rows) | Translated to `{baseURL}/responses` and back, incl. function calls and streaming. |
| OpenCode Go routing | Forwards Claude Code's native session header + always sends `x-opencode-session` (stable fallback persisted in data dir); distinctive `frank-opencode/x.y.z` User-Agent. |

## Dev mode

Prereqs: Rust stable, `opencode` v2 logged in (`opencode auth login`).

```bash
cargo test                    # unit tests (translation, aliases, config)
cargo clippy -- -D warnings   # lint (must be clean)
cargo fmt --check

cargo run -- --refresh        # print catalog: N enabled models + gateway aliases
cargo run -- --serve          # foreground server on :3737 (RUST_LOG=debug for logs)
cargo run -- --serve --port 3739

curl -s http://127.0.0.1:3737/health
curl -s "http://127.0.0.1:3737/v1/models?limit=1000" | head -c 500
```

Install the binary:

```bash
cargo install --path .
# then `frank-opencode` is on PATH
```

## Daemon

```bash
frank-opencode --enable [--port 3737]   # pidfile ~/.local/share/frank-opencode/frank.pid
frank-opencode --status
frank-opencode --disable
```

Logs: `~/.local/share/frank-opencode/frank.log`.

## Config

File: `~/.config/frank-opencode/config.toml` (see `config.example.toml`).
Env overrides: `FRANK_PORT`, `FRANK_AUTH_TOKEN`, `FRANK_CONFIG`.

## Choosing models in Claude Code — modelMap

Yes, you pick models inside Claude Code via `/model`, backed by `GET /v1/models`.

- **Automatic:** every enabled OpenCode model gets an alias `claude-<provider>-<model>`
  (the `claude-` prefix is required — Claude Code discovery only keeps ids containing
  `claude`/`anthropic`). Refresh with `--refresh` or restart.
- **Manual:** `[aliases."<gateway-id>"]` in `config.toml` wins over auto entries;
  `[disabled]` hides refs from the picker.
- `POST /v1/messages` also accepts direct refs (`opencode-go/kimi-k2.7-code`) and plain
  model ids, even if not listed.

## Claude Code setup

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3737
export ANTHROPIC_AUTH_TOKEN=dummy
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1  # enables /model picker entries
claude
```

Persist in `~/.claude/settings.json` (user scope, never the shared project file):

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3737",
    "ANTHROPIC_AUTH_TOKEN": "dummy",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
  }
}
```

Verify before opening Claude Code:

```bash
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model": "claude-opencode-go-kimi-k2-7-code", "max_tokens": 64,
       "messages": [{"role": "user", "content": "Say hi"}]}'
# -> {"id":"msg_...","type":"message",...}
```

`/status` inside Claude Code should show your `Anthropic base URL`.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `401` “no stored credential for 'X'” | Run `opencode auth login` for that provider. |
| `401` “invalid gateway credential” | `auth_token` is set in config: `ANTHROPIC_AUTH_TOKEN` must match it. |
| `MissingSessionID` from Go | frank-opencode < 0.1.1; upgrade (session headers are now automatic). |
| `FreeTierError` on `opencode/*` models | Console free tier only works inside OpenCode; those models are hidden from `/v1/models` by default (`include_free_tier = true` to show). Use an `opencode-go/*` model. |
| Empty output with tiny `max_tokens` | Reasoning models spend budget in reasoning first; raise `max_tokens` (Claude Code does this by default). |
| Models missing from `/model` | Set `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`; ids must contain `claude`/`anthropic` (auto aliases do). |
| `400` naming `context_management`/`output_config` | Upstream rejects a pre-release field; retry with `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`. |
| `"X" isn't described by this version's model catalog` | Expected: gateway ids are synthetic, so Claude Code assumes a 200k window. Map the alias with `behavesAs`/`modelOverrides` to the closest Claude model, append `[1m]` to the model name for 1M windows (the gateway strips it), or set `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1`. |
| `Waiting for API response · will retry` (stalls) | Long reasoning pauses with no stream bytes. The gateway injects `ping` frames during upstream silence; if it persists, check `frank.log` for upstream errors and consider raising `API_TIMEOUT_MS`. |

## Auto-mode notice ("session isn't eligible...")

Expected when using any gateway, not an error: Anthropic moved auto-mode classifier
checks server-side (free), but sessions routed through `127.0.0.1` can't use them —
upstream here is OpenCode Go, never the Anthropic API. Press **Enter** to continue
(snoozed 24h on that machine), or silence it permanently:

```bash
export CLAUDE_CODE_AUTO_MODE_SERVER=0
```

or in the `env` block of `~/.claude/settings.json`. Fallback classifier calls are
tiny model requests billed as normal Go usage.

## Security

- Binds `127.0.0.1` only. DB opened `READ_ONLY`. Secrets stay in memory (`secrecy`), never logged.
- State files (`frank.pid`, `frank.port`, `frank.session`, `frank.log`) are created `0600`.
- When `auth_token` is set, every endpoint except `/health` requires it
  (`x-api-key` or `Authorization: Bearer`).

## Health

`GET /health` (no auth) reports `ok` / `degraded` / `starting`, the model count,
the effective default model, and `last_error` from the background catalog
refresh (every `refresh_interval_secs`, min 60s).

## Limits (MVP)

- `#variant` suffixes ignored. `count_tokens` is a rough char-based estimate.
- Free-tier `opencode/*` models are blocked upstream outside OpenCode (hidden by default).
- `frank.log` is append-only: truncate it occasionally (`: > frank.log`).
- `--enable` refuses a different `--port` while running; `--disable` first.
