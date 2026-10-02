# frank-opencode

Gateway local compatível com Anthropic que expõe seus **modelos do OpenCode** (v2) ao **Claude Code** — sem precisar de uma chave da Anthropic.

- Escuta **apenas em `127.0.0.1`** e nunca fala com a API da Anthropic: o upstream é o backend Go/Console do OpenCode.
- Reutiliza o **login do OpenCode** já existente na máquina (lê a credencial do SQLite, em memória).
- Traduz Anthropic Messages ↔ upstream (passthrough Anthropic, OpenAI Chat Completions ou Responses API, conforme o modelo).
- Roda como daemon em background com `--enable` / `--status` / `--disable`.

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

## Instalação (release)

Baixe o binário da **última release** e instale:

```bash
# 1. Obtenha a URL do binário mais recente
URL=$(gh release view --repo marcos2872/frank-opencode --json assets \
  --jq '.assets[] | select(.name=="frank-opencode-linux-x86_64") | .url')
#    (sem gh instalado, copie o link direto da página da release)

# 2. Baixe, torne executável e mova para o PATH
curl -L "$URL" -o /tmp/frank-opencode
install -m 0755 /tmp/frank-opencode ~/.local/bin/frank-opencode

# 3. Confira a versão
frank-opencode --version
```

> Caminho alternativo: `cargo install --path .` ou `cargo install --git https://github.com/marcos2872/frank-opencode`.

### Publicando releases

As releases são publicadas automaticamente por uma **GitHub Action** sempre que uma tag `v*` é criada:

```bash
git tag v0.1.0
git push origin v0.1.0   # a action builda e anexa o binário à release
```

## Pré-requisitos

- **OpenCode v2** instalado e logado (`opencode auth login`) — é quem autentica no Console e fornece o catálogo de modelos.
- `curl` e (opcionalmente) o `gh` CLI para baixar o binário.

## Config

Arquivo: `~/.config/frank-opencode/config.toml` (veja [`config.example.toml`](config.example.toml)).
Overrides por env: `FRANK_PORT`, `FRANK_AUTH_TOKEN`, `FRANK_CONFIG`.
`FRANK_AUTH_TOKEN` (quando não-vazio) sobrescreve o `auth_token` do arquivo.

### Autenticação do gateway (`auth_token`)

Por padrão `auth_token = ""`: o gateway aceita qualquer credencial
(aceitável porque ele só escuta em `127.0.0.1`). Para exigir credencial:

```bash
# 1. Gere um token
openssl rand -hex 32

# 2. Salve no config do gateway
mkdir -p ~/.config/frank-opencode
# edite ~/.config/frank-opencode/config.toml:
#   auth_token = "SEU_TOKEN_AQUI"

# 3. Reinicie o gateway para valer (o config é lido no boot)
frank-opencode --disable
frank-opencode --enable   # ou --enable --port XXXX se usa porta custom

# 4. Use o MESMO valor no Claude Code (ver "Rodando o Claude Code" abaixo)
export ANTHROPIC_AUTH_TOKEN="SEU_TOKEN_AQUI"
```

Alternativa sem editar arquivo (teste / efêmero): exporte `FRANK_AUTH_TOKEN`
antes do `--enable` — ele sobrescreve o arquivo e é herdado pelo daemon filho.

Regras:

- Quando setado, todo endpoint exceto `GET /health` exige o token, via
  `x-api-key: <token>` **ou** `Authorization: Bearer <token>`.
- Token errado/ausente → `401 {"error":{"type":"authentication_error",...}}`.
- Trocar o token exige reiniciar (`--disable` + `--enable`); só editar o
  arquivo não afeta o daemon já rodando.

### Timeouts do upstream

O cliente HTTP do gateway separa dois orçamentos (segundos):

- `connect_timeout_secs` (padrão `30`) — conexão TCP/TLS; faz um provider
  inalcançável falhar rápido.
- `request_timeout_secs` (padrão `3600`) — tempo total da requisição, **incluindo
  streaming**. É generoso de propósito: um turno longo com reasoning pode ficar
  aberto vários minutos. Reduza só se quiser cortar sessões presas.

## Rodando o Claude Code

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

### Fixando o modelo dos subagentes

O Claude Code pode escolher automaticamente um modelo para subagentes como
`Explore` e `general-purpose`. Quando a sessão usa o frank-opencode, fixe esse
modelo em um id que exista no catálogo do gateway para evitar erros como
`model_not_found` com um id de snapshot da Anthropic que o OpenCode não oferece.

Liste os ids disponíveis e copie um deles exatamente:

```bash
curl -s http://127.0.0.1:3737/v1/models \
  | jq -r '.data[].id' \
  | sort
```

Defina o modelo antes de iniciar o Claude Code:

```bash
export CLAUDE_CODE_SUBAGENT_MODEL="SEU_ID_EXATO_DO_V1_MODELS"
export CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1
claude
```

`CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1` é recomendado: ele força o mesmo modelo
para todos os subagentes, ignorando escolhas automáticas ou configurações
individuais de agentes. O valor de `CLAUDE_CODE_SUBAGENT_MODEL` deve ser um id
retornado por `/v1/models`, incluindo o sufixo `[1m]` quando ele aparecer.

Para persistir a configuração em `~/.claude/settings.json`, adicione ao bloco
`env`:

```json
{
  "env": {
    "CLAUDE_CODE_SUBAGENT_MODEL": "SEU_ID_EXATO_DO_V1_MODELS",
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE": "1"
  }
}
```

Feche e reabra o Claude Code depois de alterar essas variáveis. Sem
`CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, uma definição de agente ou uma escolha por
invocação pode substituir o modelo padrão.

Verifique antes de abrir o Claude Code:

```bash
# 0. Catálogo carregado? status "ok" E "models" != 0 (vide troubleshooting se for)
curl -s http://127.0.0.1:3737/health       # {"status":"ok","models":58,...}  ("starting" = catálogo ainda não carregado)
curl -s http://127.0.0.1:3737/v1/models     # {"data":[{"id":"claude-...[1m]","context_window":1000000,...}],...}  ("[1m]" só quando a janela real >= 1M)

# 1. Chat de ponta a ponta
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model": "claude-opencode-go-kimi-k2-7-code", "max_tokens": 64,
       "messages": [{"role": "user", "content": "Say hi"}]}'
# -> {"id":"msg_...","type":"message",...}
```

`/status` dentro do Claude Code deve mostrar sua `Anthropic base URL`.

### Escolhendo modelos — modelMap

Você escolhe modelos dentro do Claude Code via `/model`, alimentado por `GET /v1/models`.

- **Automático:** cada modelo habilitado do OpenCode ganha um alias `claude-<provider>-<model>`
  (o prefixo `claude-` é obrigatório — a descoberta do Claude Code só mantém ids contendo
  `claude`/`anthropic`). Modelos >= 1M são anunciados como `claude-...[1m]` (único sufixo que
  o Claude lê); use essa forma com sufixo no `settings.json: model` para não aparecer como
  `Custom model` (sem sufixo funciona na API, o gateway remove ao resolver). Linhas com mesmo
  `provider/model` mas `id` distinto (ex. `opus-4.8` vs `opus-4.8-fast`) ganham aliases distintos.
  O catálogo é lido no boot com retry (veja "Solução de problemas"
  para o caso `models:0`): para refletir modelos novos/removidos depois disso, reinicie o
  gateway (`--refresh` só pré-visualiza o que o boot carregaria). Para o picker do
  Claude Desktop (que descarta ids com nomes de modelos third-party), sete
  `desktop_aliases = true` — só o id anunciado muda
  (`...-deepseek-...` vira `...-d-eepseek-...`), refs e resolução intactos.
- **Manual:** `[aliases."<gateway-id>"]` no `config.toml` tem precedência sobre os automáticos;
  `[disabled]` esconde refs do picker.
- `POST /v1/messages` também aceita refs diretas (`opencode-go/kimi-k2.7-code`) e
  model ids simples, mesmo fora da lista.

#### Variantes (`#variant`)

Modelos que declaram variantes no catálogo do OpenCode (ex.: reasoning effort)
aceitam o sufixo `#<variant>` no id — em aliases, refs diretas e ids simples
(`claude-opencode-go-my-model#high`). A variante é validada contra o catálogo
(404 com a lista de variantes disponíveis se não existir) e traduzida para o
parâmetro do wire protocol do upstream:

- **OpenAI-compatible** (`/chat/completions`): `reasoning_effort` (labels fora do
  enum da OpenAI, como `xhigh`/`max`, são saturados para `high`; `none` remove o
  parâmetro).
- **Responses API**: `reasoning` (objeto `{"effort": ...}`) mais os
  `include` declarados pela variante.
- **Passthrough Anthropic**: `thinking` derivado do que o catálogo declara para
  a variante — `{"type": "adaptive", "display": "summarized"}` para variantes
  com `thinking`, `{"type": "disabled"}` para `none`. Uma variante sem
  representação segura no Messages API (apenas `reasoningEffort`) retorna **400**
  em vez de ser silenciosamente ignorada.

#### Janela de contexto manual (quando o auto-sufixo não basta)

O gateway anuncia em `/v1/models` a janela do catálogo (`limit.context`) como
`context_window` e, para janelas >= 1M, como sufixo `[1m]` no id — o único
sufixo que a mainline do Claude Code lê. **Janelas < 1M** e o aviso
`"X" isn't described by this version's model catalog` não são resolvidos pelo
gateway; nessas situações defina a janela manualmente no lado do Claude Code:

```jsonc
// ~/.claude/settings.json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3737",
    "ANTHROPIC_AUTH_TOKEN": "dummy"
  },
  "modelOverrides": {
    "claude-opencode-go-kimi-k2-7-code": {
      "behavesAs": "claude-sonnet-4-5",   // herda janela/uso do modelo Claude mais próximo
      "inputWindowHint": 262144           // ou fixe a janela em tokens diretamente
    }
  }
}
```

Alternativas: sufixe o modelo manualmente — `"model": "claude-...[1m]"` apenas
para janelas de 1M (o gateway remove o sufixo ao resolver) — ou, como último
recurso, `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1` (desliga a
checagem de janela para ids desconhecidos, perdendo a conta real de tokens).

## Solução de problemas

| Sintoma | Causa / correção |
|---|---|
| `401` "no stored credential for 'X'" | Rode `opencode auth login` para aquele provider. |
| `400` "IDE authentication failed ... invalid token: unknown format" | Provider `github-copilot`: a credencial é envelope OAuth (`access`), não API-key (`key`). O gateway agora extrai `access`; se o erro persistir, re-autentique: `opencode auth login` e `opencode auth switch` (ajuste o `switch` para o provider copilot). |
| `400` "`X` is not accessible via the /chat/completions endpoint" | Provider `github-copilot`: GPT-6/5.6, grok, mai-code e codex são servidos pela Responses API, não Chat Completions. O gateway roteia pelo `settings.endpoint` que o catálogo declara por modelo (`"responses"`/`"chat"`/`"messages"`); reinicie o gateway para pegar o binário novo. |
| `401` "invalid gateway credential" | `auth_token` está setado no config: `ANTHROPIC_AUTH_TOKEN` precisa ser igual. |
| `MissingSessionID` do Go | frank-opencode < 0.1.1; atualize (headers de sessão agora são automáticos). |
| `FreeTierError` em modelos `opencode/*` | Free tier do Console só funciona dentro do OpenCode; esses modelos ficam ocultos de `/v1/models` por padrão (`include_free_tier = true` para exibir). Use um modelo `opencode-go/*`. |
| Saída vazia com `max_tokens` minúsculo | Modelos de reasoning gastam o orçamento no reasoning primeiro; aumente `max_tokens` (o Claude Code já faz isso por padrão). |
| Modelos faltando no `/model` | Sete `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`; ids precisam conter `claude`/`anthropic` (os aliases automáticos já contêm). |
| `/health` mostra `"models":0` e `/v1/models` vazio | O catálogo é lido no boot quando o serviço/backend do OpenCode ainda não estava pronto: `opencode api get /api/model` pode (re)iniciar o serviço, e o primeiro fetch volta com catálogo vazio sem registrar erro (`last_error:null`). O gateway agora **tenta até 6 vezes com backoff** no boot enquanto o catálogo vier vazio (e marca `/health` como `degraded` quando esgota); se mesmo assim seguir vazio, verifique `opencode service status` e reinicie com `frank-opencode --disable && frank-opencode --enable`. |
| `400` citando `context_management`/`output_config` | O upstream rejeita um campo pré-release; tente com `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`. |
| `"X" isn't described by this version's model catalog` | Esperado: ids do gateway são sintéticos e o Claude Code assume janela de 200k. O gateway anuncia a janela real do catálogo do OpenCode (`limit.context`) em `/v1/models` — como campo `context_window` e, para janelas >= 1M, como sufixo `[1m]` no próprio id (o único sufixo que a mainline do Claude Code lê; o gateway remove o sufixo ao resolver). Valide com `curl -s http://127.0.0.1:3737/v1/models | jq '.data[] | {id, context_window}'`. Janelas < 1M continuam com valor padrão no cliente: persista com `behavesAs`/`modelOverrides` para o modelo Claude mais próximo, ou em último caso `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1`. |
| `Waiting for API response · will retry` (travamentos) | Pausas longas de reasoning sem bytes no stream. O gateway injeta frames `ping` durante o silêncio do upstream; se persistir, cheque `frank.log` por erros do upstream e considere aumentar `API_TIMEOUT_MS`. |
| Modelo aparece como `Custom model` no `/model` | O picker do Claude Code faz match exato do `id`. Para janelas >= 1M o gateway anuncia `claude-...[1m]`; se seu `settings.json: model` tem a forma sem sufixo (ou vice-versa), funciona na API (o gateway remove o sufixo ao resolver) mas aparece como custom. Reseleciona a forma com `[1m]` no `/model`. |
| `400 {"model":"X"}` intermitente | Passthrough do upstream Go (ex. indisponibilidade pontual, limite, modelo em rolagem). O gateway agora loga em `frank.log` com `gateway_model/opencode_ref/base_url/status/body` para diagnóstico. Trocar de modelo e voltar costuma resolver; se persistir, reinicie o gateway. |
| Modelo removido/renomeado no `opencode-go` continua listado | Catálogo é lido só no boot: após `opencode auth` novo ou rolagem de modelos (`opus-4.7` → `opus-4.8`), rode `frank-opencode --disable && frank-opencode --enable`. |
| `Claude Opus 4.8` vs `Claude Opus 4.8 Fast` | Mesmo `modelID`, `id`/`headers` diferentes (`fast-mode-2026-02-01` + `{"speed":"fast"}`). O gateway agora gera 2 aliases distintos (`...-opus-4-8` e `...-opus-4-8-fast`) e repassa `anthropic-beta`/`speed` do catálogo. |

> **Contexto:** o Claude Code pode mostrar *"There's an issue with the selected model
> (claude-…)"* de forma intermitente mesmo com o gateway saudável. Essa mensagem é genérica —
> qualquer 4xx do upstream cujo texto cite o modelo a dispara (indisponibilidade pontual ou
> limite de uso do provedor, ex. `opencode.ai/zen/go`). Ela **não** significa que o alias sumiu
> do catálogo: como o catálogo é lido só no boot, `/v1/models` e `/health` continuam normais
> nesses momentos. Se o modelo realmente sumir do lado do provedor, reinicie o gateway.

### Aviso de auto-mode ("session isn't eligible...")

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
o modelo padrão efetivo e o `last_error` do load do catálogo no boot. No boot o catálogo
é tentado até 6 vezes com backoff crescente (~2s → 16s) enquanto vier vazio (comum quando
`opencode api` (re)inicia o serviço); esgotadas as tentativas com 0 modelos, `last_error`
fica preenchido e o status vira `degraded`, em vez de parecer `ok` com catálogo vazio.
Não há refresh automático depois do boot — para atualizar o catálogo, reinicie o gateway.

## Limites (MVP)

- `count_tokens` é estimativa local por partes (sem tokenizer BPE): texto ≈ 1 token/4 chars,
  +overhead por mensagem/tool, imagens base64 pelo tamanho real. Para pacotes Anthropic há proxy
  para `{baseURL}/messages/count_tokens` (com fallback na estimativa se o upstream falhar).
- Modelos free-tier `opencode/*` são bloqueados no upstream fora do OpenCode (ocultos por padrão).
- `frank.log` é só-append: trunque de vez em quando (`: > frank.log`).
- `--enable` recusa uma `--port` diferente com ele rodando; dê `--disable` antes.

## Desenvolvimento

CI (GitHub Actions) roda formatação, lint e a suíte completa em todo push e PR.
A branch `main` está protegida: o status **test** é obrigatório e a branch precisa
estar atualizada — uma PR só mergeia com os testes verdes. Há também um hook de
pre-commit opcional; ative com `git config core.hooksPath .githooks`. Detalhes em
[docs/dev.md](docs/dev.md).

Há ainda um teste de latência do proxy (`tests/perf.rs`, Claude-simulador →
gateway → OpenCode-simulador) com um job de CI que exige `p95 < 15ms` em release —
ver [docs/dev.md](docs/dev.md#performance).

## Documentação

- [Desenvolvimento](docs/dev.md) — como rodar em dev, testes, lint.
- [Arquitetura](docs/arquitetura.md) — como o gateway é estruturado (domínio, infra, API).
- [Claude Desktop](docs/desktop.md) — configuração no app Desktop (pesquisa não testada).