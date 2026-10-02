# Desenvolvimento

Pré-requisitos: Rust stable, `opencode` v2 logado (`opencode auth login`).

## Comandos

```bash
cargo test                    # testes unitários (tradução, aliases, config) + e2e (tests/gateway.rs)
cargo test --test gateway     # só os testes e2e do gateway
cargo test --test perf        # latência de tradução do proxy (report-only em debug)
cargo test <name>             # teste único por substring do nome
cargo clippy -- -D warnings   # lint (precisa estar limpo)
cargo fmt --check             # formatação (precisa estar limpa)
```

## CI e hooks

O workflow [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) roda em todo
push e em toda PR: `cargo fmt --all --check`, `cargo clippy --all-targets --
-D warnings` e `cargo test --all-targets`. Um job separado `perf` (após o `test`)
compila em release e força o orçamento de latência (ver abaixo). A branch `main`
está protegida: o status **test** é obrigatório e a branch precisa estar
atualizada — **uma PR não mergeia enquanto os testes não passarem** (o admin
ainda pode dar push direto).

Para reproduzir o CI localmente, os mesmos comandos acima com `--all-targets`.

Cada job publica um **resumo** ao final (painel *Summary* do GitHub Actions): o
`test` mostra o status de cada etapa (fmt/clippy/testes) e o total de testes; o
`perf` mostra a tabela p50/p95/etc. por cenário e o veredito do orçamento.

Há um hook de pre-commit em [`.githooks/pre-commit`](../.githooks/pre-commit) que
roda `fmt --check`, `clippy` e `test` antes de cada commit. Ele **não** é ativado
automaticamente (o Git não versiona `.git/hooks`); habilite uma vez por clone:

```bash
git config core.hooksPath .githooks
```

Para pular um commit específico: `git commit --no-verify`.

## Performance

`tests/perf.rs` mede a **latência de tradução do proxy** no formato
Claude-simulador → gateway → OpenCode-simulador:

- O cliente é o próprio teste, via `axum_test::TestServer` in-process (o hop
  cliente→proxy fica **fora** da métrica).
- O upstream é um app axum mock num `TcpListener` real de loopback, então
  `reqwest`/serialização/pool de conexões entram na conta — é o caminho de
  produção. O payload é realista (~8–16 KB: system longo, par tool_use/tool_result
  e 2 tools) para a tradução não ser trivial.
- Cada amostra cronometra `POST /v1/messages` (não-streaming) inteiro. Após um
  warmup descartado (absorve o 1º connect), reporta p50/p95/mean/min/max.

Enforcement: builds **release** exigem `p95 <= 15ms` (job `perf` do CI, com
`--test-threads=1` para os dois cenários não disputarem CPU). Builds **debug** —
como o `cargo test --all-targets` do job `test` — rodam e reportam, sem reprovar
(o timings sem otimização é ruidoso demais para gate). Para reproduzir ou ajustar:

```bash
cargo test --test perf -- --nocapture                                    # debug: só reporta
cargo test --release --test perf -- --nocapture --test-threads=1         # release: força p95 < 15ms
FRANK_PERF_P95_MS=25 cargo test --release --test perf                    # sobrepõe o limiar
```

Para encarecer o cenário, aumente `FILLER_LINES` em `realistic_body`.

## Rodando

```bash
cargo run -- --refresh        # imprime catálogo: N modelos habilitados + aliases do gateway
RUST_LOG=warn cargo run -- --serve          # servidor em foreground na :3737 (RUST_LOG=debug p/ logs)
cargo run -- --serve --port 3739
```

Verificando o servidor:

```bash
curl -s http://127.0.0.1:3737/health
curl -s "http://127.0.0.1:3737/v1/models?limit=1000" | head -c 500
```

## Resetando o cache do Claude Code (dev)

O discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) busca
`GET /v1/models` a cada startup do Claude e grava em
`~/.claude/cache/gateway-models.json`. O catálogo do gateway é lido só no
boot, então após mudar aliases/código:

```bash
# 1. Gateway com catálogo fresco (mata o --serve antigo e sobe de novo)
curl -s http://127.0.0.1:3737/health  # confira "models" != 0
# daemon: frank-opencode --disable && frank-opencode --enable
# foreground: cargo run -- --serve --port 3737

# 2. Confere o novo mapeamento (zero dup, fast com alias próprio)
curl -s http://127.0.0.1:3737/v1/models \
  | jq -r '.data[].id' | sort | uniq -d  # vazio = ok

# 3. Força o Claude a reler (saia do claude antes)
rm ~/.claude/cache/gateway-models.json
claude
# /model -> reselecione o id COM [1m], ex. claude-opencode-go-deepseek-v4-1-flash[1m]
```

Não apague `~/.claude/cache/model-catalog/` (catálogo publicado da
Anthropic, não do gateway). Se o picker mostra `Custom model`, o
`settings.json: model` tem a forma sem `[1m]` — funciona na API (o gateway
remove o sufixo ao resolver) mas o picker compara string exata; reselecionar
no `/model` reescreve o campo.

## Instalando o binário local

```bash
cargo install --path .
# depois `frank-opencode` fica no PATH
```

## Daemon

```bash
frank-opencode --enable [--port 3737]   # pidfile ~/.local/share/frank-opencode/frank.pid
frank-opencode --status
frank-opencode --disable
```

Logs: `~/.local/share/frank-opencode/frank.log`.

## Estado de build

- `cargo clippy -- -D warnings` e `cargo fmt --check` precisam ficar limpos (como no CI).
- Os testes e2e (`tests/gateway.rs`) usam um upstream mock — nunca chamam o binário real nem a rede.
- O model catalog vem de `opencode api get /api/model`; os testes de servidor usam um `AppState` semeados.
