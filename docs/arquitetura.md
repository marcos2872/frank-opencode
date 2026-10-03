# Arquitetura

[← README](../README.md) · [Configuração](configuracao.md) · [CLI](config-cli.md) ·
[Desktop](config-desktop.md) · [Erros](erros.md) · [Dev](dev.md) ·
**Arquitetura**

`frank-opencode` é um gateway local compatível com Anthropic que expõe os
modelos do OpenCode v2 ao Claude Code. Escuta apenas em `127.0.0.1` e **nunca**
fala com a Anthropic — o upstream é o backend Go/Console do OpenCode, roteado
pelo pacote (provider).

```mermaid
flowchart LR
    CC["Claude Code"]
    GW["frank-opencode<br/>(127.0.0.1:3737)"]
    UP["OpenCode Go backend"]
    DB[("SQLite<br/>(credential)")]

    CC -->|"Anthropic Messages"| GW
    GW -->|"resolve model → protocol_for → traduz → aplica variante"| UP
    UP -.->|"credencial (só-leitura)"| DB
```

## Camadas

- **`domain/`** — tipos e regras puras, sem async. `ModelRef` (provider/model,
  sufixo `#variant`), `CatalogEntry` (deserializa a saída de
  `opencode api get /api/model`, incl. `variants`), `protocol_for(package)` →
  protocolo de wire, `auto_alias` (o id do gateway precisa conter
  `claude`/`anthropic` para a descoberta de `/v1/models` do Claude Code),
  `strip_window_suffix` (`[1m]`/`[200k]` hints que o Claude Code anexa a ids desconhecidos).
- **`infra/opencode.rs`** — estado do OpenCode. Credenciais **somente** da tabela
  `credential` da SQLite (read-only; `auth.json` nunca é fonte de verdade; nunca
  parseia `opencode.jsonc`). Catálogo via `opencode api get /api/model`. Caminho
  do DB via `opencode debug paths db`.
- **`infra/upstream.rs`** — tradutores puros (sem HTTP):
  - `anthropic_to_openai` / `openai_to_anthropic` (Chat Completions)
  - `anthropic_to_responses` / `responses_to_anthropic` (Responses API)
  - `StreamTranslator` / `ResponsesTranslator` (SSE → Anthropic SSE)
  - `apply_variant` — mescla o `reasoning_effort` da variante selecionada no
    body **já traduzido** (os tradutores descartam campos desconhecidos). Chave
    por protocolo: Chat = `reasoning_effort` (labels fora do enum da OpenAI
    saturam para `high`), Responses = `reasoning`, Anthropic = no-op.
  - `estimate_tokens` — contagem local por partes para `count_tokens` (sem tokenizer).
  - `with_heartbeat` — injeta `event: ping` durante o silêncio do upstream.
- **`api/server.rs`** — handlers Axum. Fluxo do body: resolve model → escolhe o
  forward por `protocol_for` → traduz → aplica variante → forward com o
  `Bearer` da credencial + headers de sessão (`x-opencode-session`, sempre enviado).

## Convenções e pegadinhas

- **Resolução de modelo** (`AppState::resolve`): alias → `provider/model` → id
  simples (ids ambíguos ordenam por nome qualificado, com aviso). Variantes são
  validadas contra o catálogo; label desconhecido → 404 listando as disponíveis.
- **Free-tier**: modelos `opencode/*` dão 403 fora do OpenCode, então ficam
  ocultos de `/v1/models` a menos que `include_free_tier = true`. Usam a chave
  pública de `settings.apiKey` (`upstream_bearer`).
- **Headers de sessão**: o forward para o Go sempre envia `x-opencode-session`
  (o `x-claude-code-session-id` do cliente, depois `x-opencode-session`, senão um
  fallback persistido em `frank.session`).
- **count_tokens**: pacotes Anthropic fazem proxy para
  `{baseURL}/messages/count_tokens` com fallback em `estimate_tokens`; os outros
  pacotes sempre estimam localmente.
- **Formato de erro**: sempre `{"type":"error","error":{"type":...,"message":...}}`
  com tipos de erro Anthropic (`not_found_error`, `authentication_error`,
  `invalid_request_error`, `api_error`).
- **Refresh do catálogo** roda em background após o bind; `/health` fica
  `starting` até o primeiro sucesso, `degraded` após falha, `ok` caso contrário.
  Os testes constroem um `AppState` semeado e nunca chamam o binário real.

## Daemon e ciclo de vida

- `frank-opencode --enable` escreve pidfile/porta/sessão em
  `~/.local/share/frank-opencode/` (arquivos `0600` via `daemon::write_private`)
  e spawna o filho com `--daemon-child`.
- `--disable` envia SIGTERM (drena os streams SSE em andamento) e limpa o estado.
- `--status` reporta se o daemon está rodando, em qual porta e o session id.

## Limites conhecidos

- `count_tokens` é estimativa local por partes (sem tokenizer BPE): texto ≈ 1 token/4 chars,
  +overhead por mensagem/tool, imagens base64 pelo tamanho real. Para pacotes Anthropic há proxy
  para `{baseURL}/messages/count_tokens` (com fallback na estimativa se o upstream falhar).
- Modelos free-tier `opencode/*` são bloqueados no upstream fora do OpenCode (ocultos por padrão).
- `frank.log` é só-append: trunque de vez em quando (`: > frank.log`).
- `--enable` recusa uma `--port` diferente com ele rodando; dê `--disable` antes.

## Documentação relacionada

- [Desenvolvimento](dev.md) — building, testes, comandos.
- [Configuração](configuracao.md) — `config.toml`, aliases e variantes.
- [Erros e diagnóstico](erros.md) — saúde, logs e sintomas.
- [Setup no Claude Code CLI](config-cli.md) — config e uso no dia a dia.
- [Setup no Claude Desktop](config-desktop.md) — o app Desktop.