# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo test                    # unit + integration tests (tests/gateway.rs)
cargo test --test gateway     # only the e2e gateway tests
cargo test <name>             # single test by name substring
cargo clippy -- -D warnings   # must stay clean
cargo fmt --check             # must stay clean
cargo run -- --refresh        # print catalog + gateway aliases, exit
cargo run -- --serve          # foreground server on 127.0.0.1:3737
```

Daemon lifecycle: `frank-opencode --enable | --disable | --status`. State files
(pid, port, session id, log) live in `~/.local/share/frank-opencode/` and are
created 0600 via `daemon::write_private`.

## Architecture

`frank-opencode` is a local Anthropic-compatible gateway that exposes OpenCode
v2's models to Claude Code. It listens only on `127.0.0.1` and never talks to
Anthropic — the upstream is the OpenCode Go/Console backend routed by provider
package.

Layers:

- **`domain/`** — pure types and rules, no async. `ModelRef` (provider/model,
  `#variant` suffix), `CatalogEntry` (deserializes `opencode api get /api/model`
  output, including `variants`), `protocol_for(package)` → wire protocol,
  `auto_alias` (gateway id must contain `claude`/`anthropic` for Claude Code's
  `/v1/models` discovery), `strip_window_suffix` (`[1m]`/`[200k]` hints Claude
  Code appends to unknown ids).
- **`infra/opencode.rs`** — OpenCode state. Credentials ONLY from the SQLite
  `credential` table (read-only; never `auth.json` as source of truth; never
  parse `opencode.jsonc`). Catalog via `opencode api get /api/model`. DB path
  via `opencode debug paths db`.
- **`infra/upstream.rs`** — pure translators (no HTTP):
  - `anthropic_to_openai` / `openai_to_anthropic` (Chat Completions)
  - `anthropic_to_responses` / `responses_to_anthropic` (Responses API)
  - `StreamTranslator` / `ResponsesTranslator` (SSE → Anthropic SSE)
  - `apply_variant` — merge the selected variant's `reasoning_effort` into the
    **translated** body (translators drop unknown fields). Key per protocol:
    Chat = `reasoning_effort` (labels outside OpenAI's enum saturate to `high`),
    Responses = `reasoning`, Anthropic = no-op.
  - `estimate_tokens` — per-part local count for `count_tokens` (no tokenizer).
  - `with_heartbeat` — injects `event: ping` during upstream silence.
- **`api/server.rs`** — Axum handlers. `body` flows: resolve model → pick
  forward by `protocol_for` → translate → applies variant → forward with
  `Bearer` credential + session headers (`x-opencode-session`, always sent).

## Conventions & gotchas

- **Model resolution** (`AppState::resolve`): alias → `provider/model` → plain
  model id (ambiguous ids sort by qualified name, warn). Variants validated
  against the catalog; unknown label → 404 listing available ones.
- **Free-tier**: `opencode/*` models 403 outside OpenCode, so they are hidden
  from `/v1/models` unless `include_free_tier = true`. They use the public key
  from `settings.apiKey` (`upstream_bearer`).
- **Session headers**: forwarding to Go always sends `x-opencode-session`
  (client's `x-claude-code-session-id`, then `x-opencode-session`, else a
  persisted fallback in `frank.session`).
- **count_tokens**: Anthropic packages proxy to `{baseURL}/messages/count_tokens`
  with fallback to `estimate_tokens`; other packages always estimate locally.
- **Error shape**: always `{"type":"error","error":{"type":...,"message":...}}`
  with Anthropic error types (`not_found_error`, `authentication_error`,
  `invalid_request_error`, `api_error`).
- **Catalog is read once** in a background task after bind (no periodic refresh;
  remove/restart to pick up new models, `--refresh` previews what boot would
  load). `/health` stays `starting` until that first load, `degraded` after a
  failure, `ok` otherwise. Tests build a seeded `AppState` and never call the
  real binary.
- **Docs live in README.md** (Portuguese): config, modelMap/aliases, setup in
  Claude Code, troubleshooting. Keep it in sync when behavior changes.