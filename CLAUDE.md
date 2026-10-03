# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo test                    # unit + integration tests (tests/gateway.rs)
cargo test --test gateway     # only the e2e gateway tests
cargo test --test perf        # proxy translation latency (report-only in debug)
cargo test <name>             # single test by name substring
cargo clippy -- -D warnings   # must stay clean
cargo fmt --check             # must stay clean
cargo run -- --refresh        # print catalog + gateway aliases, exit
cargo run -- --serve          # foreground server on 127.0.0.1:3737
```

CI: `.github/workflows/ci.yml` runs `cargo fmt --all --check`, `cargo clippy
--all-targets -- -D warnings` and `cargo test --all-targets` on every push and
PR. Steps use `continue-on-error` + a final `Propagar falha` step so the job
summary and sticky PR comment (`.github/scripts/pr-sticky-comment.sh`) always
publish even when steps fail. A separate `perf` job builds `--release` and
enforces per-scenario p95 budgets (`tests/perf.rs`: protocol × input-shape
matrix, 15ms light / 25ms heavy; `FRANK_PERF_P95_MS` overrides all; scenario
labels must stay `[a-z_]+` or the summary regex misses them). `main` is protected — the `test`
check is required and the branch must be up to date, so a PR cannot merge with
failing tests (admins may still push directly). An opt-in pre-commit hook
(`.githooks/pre-commit`, same three commands) is enabled per clone with
`git config core.hooksPath .githooks`.

Daemon lifecycle: `frank-opencode --enable [--port PORT] | --disable | --status`. Configuration is loaded from `~/.config/frank-opencode/config.toml` (or `--config`/`FRANK_CONFIG`), with `FRANK_PORT` and `FRANK_AUTH_TOKEN` overrides. State files
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
  output, including `variants`, per-model headers/body defaults, context limits,
  and distinct catalog `id` rows), `protocol_for_entry` → wire protocol (including
  `settings.endpoint` for mixed `github-copilot` catalogs), `auto_alias` (gateway
  id must contain `claude`/`anthropic` for Claude Code's `/v1/models` discovery),
  `shield_cli_family_match` (automatic family-spelling rewrite on advertised ids —
  `claude-sonnet`→`cs`, `claude-opus`→`co`, plus `haiku`/`fable`/`mythos` — via
  default-on `cli_shield_aliases`, so CLI background calls stop landing on those
  rows), optional `desktop_aliases` rewriting for Claude Desktop's denylist, and
  `strip_window_suffix` (`[1m]`/`[200k]` hints Claude Code appends to unknown ids).
- **`infra/opencode.rs`** — OpenCode state. Credentials ONLY from the SQLite
  `credential` table (read-only; never `auth.json` as source of truth; never
  parse `opencode.jsonc`). Catalog via `opencode api get /api/model`. DB path
  via `opencode debug paths db`.
- **`infra/upstream.rs`** — pure translators (no HTTP):
  - `anthropic_to_openai` / `openai_to_anthropic` (Chat Completions)
  - `anthropic_to_responses` / `responses_to_anthropic` (Responses API)
  - `StreamTranslator` / `ResponsesTranslator` (SSE → Anthropic SSE)
  - `apply_variant` / `apply_variant_checked` — merge the selected variant into
    the **translated** body (translators drop unknown fields). Key per protocol:
    Chat = `reasoning_effort` (removed for `none`), Responses = `reasoning`
    object + `include`, Anthropic = `thinking` object (`adaptive`/`disabled`)
    from the variant's `thinking`/`effort`. A Messages-API variant with no
    representable field (`reasoningEffort` alone) is a 400, not a silent no-op.
  - catalog defaults — merge per-model `headers` and `body` fields (for example
    fast-mode beta headers and `speed`) into forwarded requests.
  - `floor_output_tokens` — raises `max_tokens` / `max_output_tokens` below 16
    to 16 on translated protocols (the zen backend rejects smaller output
    limits; Claude Code probes model switches with `max_tokens: 1`). The
    Anthropic passthrough path is untouched.
  - `estimate_tokens` — per-part local count for `count_tokens` (no tokenizer).
  - `with_heartbeat` — injects `event: ping` during upstream silence.
  - `sse_error` — Anthropic mid-stream `error` event, used when the upstream
    stream fails or reports `response.failed` after opening.
- **`api/server.rs`** — Axum handlers. `body` flows: resolve model → pick
  forward by `protocol_for_entry` (catalog `settings.endpoint` can override the
  package for mixed `github-copilot` rows) → translate → merge catalog defaults
  → apply variant → forward with `Bearer` credential + session headers
  (`x-opencode-session`, always sent). The reqwest client uses a short
  `connect_timeout` and a generous total `timeout` (`connect_timeout_secs` /
  `request_timeout_secs`), because the total also bounds streaming responses.
  Non-2xx upstream bodies are normalized to the Anthropic error shape
  (`upstream_error_response`), keeping the raw body only in the log — the
  `upstream rejected request` WARN also records `user_agent` and `session`
  (`x-claude-code-session-id`) so background callers can be attributed.

## Conventions & gotchas

- **Model resolution** (`AppState::resolve`): alias → `provider/model` → plain
  model id (ambiguous ids sort by qualified name, warn). Variants validated
  against the catalog; unknown label → 404 listing available ones. An empty
  `model` falls back to `config.default_model` (`effective_default`); with
  neither set it is a 400.
- **mock_classifier** (`config.toml`, default `false`): answers Claude Code's
  auto-mode safety-classifier checks and liveness probes locally, before
  `resolve` and only when `!stream` — real conversations (which carry tools)
  are never matched and still forward. Does NOT cover Claude Desktop's
  background title calls (different prompt, no `<block>`/`<severity>` tags).
- **`[tiers]`** (`config.toml`, default empty): Anthropic family tiers announced
  on `/v1/models` (`anthropic_family_tier` + `is_family_default`). Key = gateway
  id or `provider/model` ref. Desktop's `small_fast` background class picks the
  first `haiku`, then `sonnet`, then `opus` — unmapped aliases announce no tier.
  **Claude Code CLI ignores these**: its discovery cache schema strips to
  `{id, display_name, description}` + window, so it resolves background models
  by id-substring family spelling (which lands on the Copilot rows). The CLI is
  covered by the automatic `cli_shield_aliases` rewrite instead (default on;
  docs "Blindando o Copilot", `cs-`/`co-` ids) plus `ANTHROPIC_SMALL_FAST_MODEL`
  in `~/.claude/settings.json`. Prefer `[tiers]` keys by `provider/model` ref,
  which survive the automatic renames.
- **Free-tier**: `opencode/*` models 403 outside OpenCode, so they are hidden
  from `/v1/models` unless `include_free_tier = true`. They use the public key
  from `settings.apiKey` (`upstream_bearer`).
- **Session headers**: forwarding to Go always sends `x-opencode-session`
  (client's `x-claude-code-session-id`, then `x-opencode-session`, else a
  persisted fallback in `frank.session`).
- **count_tokens**: Anthropic packages proxy to `{baseURL}/messages/count_tokens`
  with fallback to `estimate_tokens`; other packages always estimate locally.
  The proxy applies the resolved variant and the catalog body defaults so the
  count matches the real forward, and forwards the client's `anthropic-beta` /
  `anthropic-version` and session headers.
- **Error shape**: always `{"type":"error","error":{"type":...,"message":...}}`
  with Anthropic error types (`not_found_error`, `authentication_error`,
  `invalid_request_error`, `api_error`).
- **Catalog is read once** in a background task after bind (no periodic refresh;
  remove/restart to pick up new models, `--refresh` previews what boot would
  load). `/health` stays `starting` until that first load, `degraded` after a
  failure, `ok` otherwise. Tests build a seeded `AppState` and never call the
  real binary.
- **Docs are split by audience under `docs/`** (Portuguese): `README.md` is only
  the project presentation + binary install + links; `docs/configuracao.md`
  (gateway `config.toml`, auth, aliases, variants), `docs/config-cli.md` (Claude
  Code CLI setup), `docs/config-desktop.md` (Claude Desktop + `[tiers]`),
  `docs/erros.md` (health/logs/troubleshooting), `docs/dev.md` (dev, CI,
  releases) and `docs/arquitetura.md` (layers, MVP limits). Keep them in sync
  when behavior changes, and preserve the top-of-file nav line and the
  "Veja também" cross-links between them.