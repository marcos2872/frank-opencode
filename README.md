# frank-opencode

Gateway local compatível com Anthropic que expõe seus **modelos do OpenCode** (v2) ao **Claude Code**.

Roda apenas em `127.0.0.1`, reutiliza o login do OpenCode já existente na máquina e traduz
Anthropic Messages ↔ upstream (passthrough Anthropic ou OpenAI Chat Completions).

```bash
frank-opencode --enable    # inicia em background
frank-opencode --status
frank-opencode --disable   # para
```

## Como funciona

| Assunto | Implementação |
|---|---|
| Credenciais (OpenCode v2) | Leitura somente do SQLite `opencode.db`, tabela `credential` (envelope `{"type","key"}` desembrulhado em memória, nunca logado). Caminho via `opencode debug paths db`, senão `OPENCODE_DB` / `XDG_DATA_HOME`. `auth.json` serve só como entrada de migração legada. |
| Catálogo de modelos | `opencode api get /api/model` (com service-auth), somente `enabled`. |
| Modelos de pacote Anthropic (MiniMax, Qwen…) | Passthrough de bytes para `{baseURL}/messages` (`anthropic-version`/`anthropic-beta` repassados). |
| Modelos OpenAI-compatible (Kimi, GLM, DeepSeek…) | Traduzidos para `{baseURL}/chat/completions` e de volta, incl. `tool_use`/`tool_result`, imagens, streaming SSE. |
| Modelos Responses-API (linhas Go GPT/Grok/Muse) | Traduzidos para `{baseURL}/responses` e de volta, incl. function calls e streaming. |
| Roteamento do OpenCode Go | Repassa o header nativo de sessão do Claude Code + sempre envia `x-opencode-session` (fallback estável persistido no data dir); User-Agent distintivo `frank-opencode/x.y.z`. |

## Modo dev

Pré-requisitos: Rust stable, `opencode` v2 logado (`opencode auth login`).

```bash
cargo test                    # testes unitários (tradução, aliases, config)
cargo clippy -- -D warnings   # lint (precisa estar limpo)
cargo fmt --check

cargo run -- --refresh        # imprime catálogo: N modelos habilitados + aliases do gateway
cargo run -- --serve          # servidor em foreground na :3737 (RUST_LOG=debug p/ logs)
cargo run -- --serve --port 3739

curl -s http://127.0.0.1:3737/health
curl -s "http://127.0.0.1:3737/v1/models?limit=1000" | head -c 500
```

Instalando o binário:

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

## Config

Arquivo: `~/.config/frank-opencode/config.toml` (ver `config.example.toml`).
Overrides por env: `FRANK_PORT`, `FRANK_AUTH_TOKEN`, `FRANK_CONFIG`.

## Escolhendo modelos no Claude Code — modelMap

Sim, você escolhe modelos dentro do Claude Code via `/model`, alimentado por `GET /v1/models`.

- **Automático:** cada modelo habilitado do OpenCode ganha um alias `claude-<provider>-<model>`
  (o prefixo `claude-` é obrigatório — a descoberta do Claude Code só mantém ids contendo
  `claude`/`anthropic`). Atualize com `--refresh` ou reinicie.
- **Manual:** `[aliases."<gateway-id>"]` no `config.toml` tem precedência sobre os automáticos;
  `[disabled]` esconde refs do picker.
- `POST /v1/messages` também aceita refs diretas (`opencode-go/kimi-k2.7-code`) e
  model ids simples, mesmo fora da lista.

## Setup no Claude Code

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3737
export ANTHROPIC_AUTH_TOKEN=dummy
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1  # habilita entradas no picker /model
claude
```

Persistindo em `~/.claude/settings.json` (escopo do usuário, nunca no arquivo compartilhado do projeto):

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3737",
    "ANTHROPIC_AUTH_TOKEN": "dummy",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
  }
}
```

Verifique antes de abrir o Claude Code:

```bash
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model": "claude-opencode-go-kimi-k2-7-code", "max_tokens": 64,
       "messages": [{"role": "user", "content": "Say hi"}]}'
# -> {"id":"msg_...","type":"message",...}
```

`/status` dentro do Claude Code deve mostrar sua `Anthropic base URL`.

## Solução de problemas

| Sintoma | Causa / correção |
|---|---|
| `401` "no stored credential for 'X'" | Rode `opencode auth login` para aquele provider. |
| `401` "invalid gateway credential" | `auth_token` está setado no config: `ANTHROPIC_AUTH_TOKEN` precisa ser igual. |
| `MissingSessionID` do Go | frank-opencode < 0.1.1; atualize (headers de sessão agora são automáticos). |
| `FreeTierError` em modelos `opencode/*` | Free tier do Console só funciona dentro do OpenCode; esses modelos ficam ocultos de `/v1/models` por padrão (`include_free_tier = true` para exibir). Use um modelo `opencode-go/*`. |
| Saída vazia com `max_tokens` minúsculo | Modelos de reasoning gastam o orçamento no reasoning primeiro; aumente `max_tokens` (o Claude Code já faz isso por padrão). |
| Modelos faltando no `/model` | Sete `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`; ids precisam conter `claude`/`anthropic` (os aliases automáticos já contêm). |
| `400` citando `context_management`/`output_config` | O upstream rejeita um campo pré-release; tente com `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`. |
| `"X" isn't described by this version's model catalog` | Esperado: ids do gateway são sintéticos, então o Claude Code assume janela de 200k. Mapeie o alias com `behavesAs`/`modelOverrides` para o modelo Claude mais próximo, sufixe `[1m]` no nome do modelo para janelas de 1M (o gateway remove o sufixo), ou sete `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1`. |
| `Waiting for API response · will retry` (travamentos) | Pausas longas de reasoning sem bytes no stream. O gateway injeta frames `ping` durante o silêncio do upstream; se persistir, cheque `frank.log` por erros do upstream e considere aumentar `API_TIMEOUT_MS`. |

## Aviso de auto-mode ("session isn't eligible...")

Esperado ao usar qualquer gateway, não é erro: a Anthropic moveu as checagens
do classificador do auto-mode para server-side (grátis), mas sessões roteadas via
`127.0.0.1` não conseguem usá-las — o upstream aqui é o OpenCode Go, nunca a API
da Anthropic. Pressione **Enter** para continuar (suspenso por 24h naquela máquina),
ou silencie permanentemente:

```bash
export CLAUDE_CODE_AUTO_MODE_SERVER=0
```

ou no bloco `env` do `~/.claude/settings.json`. Chamadas do classificador de fallback são
requisições minúsculas cobradas como uso normal do Go.

## Segurança

- Só escuta em `127.0.0.1`. DB aberto em `READ_ONLY`. Segredos ficam só em memória (`secrecy`), nunca logados.
- Arquivos de estado (`frank.pid`, `frank.port`, `frank.session`, `frank.log`) criados com `0600`.
- Quando `auth_token` está setado, todo endpoint exceto `/health` o exige
  (`x-api-key` ou `Authorization: Bearer`).

## Health

`GET /health` (sem auth) reporta `ok` / `degraded` / `starting`, a contagem de modelos,
o modelo padrão efetivo e o `last_error` do refresh do catálogo em background
(a cada `refresh_interval_secs`, mín. 60s).

## Limites (MVP)

- Sufixos `#variant` ignorados. `count_tokens` é estimativa aproximada por caracteres.
- Modelos free-tier `opencode/*` são bloqueados no upstream fora do OpenCode (ocultos por padrão).
- `frank.log` é só-append: trunque de vez em quando (`: > frank.log`).
- `--enable` recusa uma `--port` diferente com ele rodando; dê `--disable` antes.
