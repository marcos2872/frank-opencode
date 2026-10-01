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