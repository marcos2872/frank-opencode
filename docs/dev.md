# Desenvolvimento

Pré-requisitos: Rust stable, `opencode` v2 logado (`opencode auth login`).

## Comandos

```bash
cargo test                    # testes unitários (tradução, aliases, config) + e2e (tests/gateway.rs)
cargo test --test gateway     # só os testes e2e do gateway
cargo test <name>             # teste único por substring do nome
cargo clippy -- -D warnings   # lint (precisa estar limpo)
cargo fmt --check             # formatação (precisa estar limpa)
```

## Rodando

```bash
cargo run -- --refresh        # imprime catálogo: N modelos habilitados + aliases do gateway
cargo run -- --serve          # servidor em foreground na :3737 (RUST_LOG=debug p/ logs)
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