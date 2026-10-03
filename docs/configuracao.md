# Configuração do gateway

[← README](../README.md) · **Configuração** · [CLI](config-cli.md) ·
[Desktop](config-desktop.md) · [Erros](erros.md) · [Dev](dev.md) ·
[Arquitetura](arquitetura.md)

Referência das opções do `config.toml`, dos overrides por variável de ambiente e de
como o gateway resolve modelos e aliases.

## Arquivo e overrides

Arquivo: `~/.config/frank-opencode/config.toml` (veja
[`config.example.toml`](../config.example.toml)).
Overrides por env: `FRANK_PORT`, `FRANK_AUTH_TOKEN`, `FRANK_CONFIG`.
`FRANK_AUTH_TOKEN` (quando não-vazio) sobrescreve o `auth_token` do arquivo.

O config é lido **no boot** — mudanças exigem reiniciar o gateway
(`frank-opencode --disable && frank-opencode --enable`).

## Autenticação do gateway (`auth_token`)

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

# 4. Use o MESMO valor no Claude Code (ver docs/config-cli.md)
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

## Modelo padrão (`default_model`)

Quando o cliente POSTa sem `"model"`, o gateway usa o `default_model` do
`~/.config/frank-opencode/config.toml` — **não** o `model` do
`~/.claude/settings.json` (um diz o que o gateway usa no fallback, o outro
o que o Claude pede). Vazio = primeiro alias em ordem alfabética, que hoje
costuma ser um `claude-github-copilot-...` (`g` < `o`), não o seu modelo de
chat. Fixe um que funciona:

```toml
default_model = "claude-opencode-go-muse-spark-1-3-contributor"
```

Reinicie o gateway (o config é lido no boot) e confira:

```bash
curl -s http://127.0.0.1:3737/health | jq '{default_model, models}'
curl -s http://127.0.0.1:3737/v1/models | jq -r '.data[0].id'
```

Se o `data[0].id` for um modelo do Copilot e o `default_model` estiver
vazio, qualquer chamada auxiliar sem `model` (probes do Claude, sem `tools`)
cai nele — é a origem dos `WARN upstream rejected request … 400
model_not_supported` "fantasmas" mesmo sem você selecionar o Copilot.
Veja [Erros](erros.md) para a distinção das origens.

## Timeouts do upstream

O cliente HTTP do gateway separa dois orçamentos (segundos):

- `connect_timeout_secs` (padrão `30`) — conexão TCP/TLS; faz um provider
  inalcançável falhar rápido.
- `request_timeout_secs` (padrão `3600`) — tempo total da requisição, **incluindo
  streaming**. É generoso de propósito: um turno longo com reasoning pode ficar
  aberto vários minutos. Reduza só se quiser cortar sessões presas.

## Mock do classificador do auto-mode (`mock_classifier`)

O Claude Code roda um classificador de segurança de dois estágios no auto-mode,
com ids auxiliares hardcoded (`claude-sonnet-5`, `claude-opus-4-8`) — além de
probes de disponibilidade de `max_tokens: 1`. Essas chamadas resolvem no catálogo
(e.g. a linha do GitHub Copilot) e, sem quota, geram WARN `upstream rejected
request … 429 quota exceeded` a cada verificação, gastando quota à toa.

Com `mock_classifier = true` o gateway responde essas verificações **localmente**
(nunca toca o upstream):

- estágio 1 → `<severity>0</severity>` (abaixo do limiar ⇒ libera sem estágio 2);
- estágio 2 → `<block>no</block>` (resposta começa com `<block>`, como o parser exige);
- probe de 1 token → `ok`.

A detecção lê só o `system` (marcadores `<severity>`/`<block>`) e exige `tools`
vazio — conversas reais (que carregam tools e o system prompt do Claude Code)
continuam encaminhadas normalmente, inclusive no modelo do Copilot selecionado.

> **Tradeoff:** com o mock ativo, a revisão de segurança do auto-mode sempre
> responde "allow" — equivalente a rodar o auto-mode sem o classificador LLM.
> As regras de permissão/hooks continuam valendo. Off por padrão.

> **Limite conhecido:** o mock só casa o classificador do Claude Code e seus
> probes. As chamadas de fundo do **Claude Desktop** (título da sessão, `max_tokens: 200`,
> sem `tools`) usam outro prompt e passam direto — para essas, use os tiers
> em [Configuração do Desktop](config-desktop.md#tiers-da-família-anthropic), não o mock.

## Escolhendo modelos — modelMap

Você escolhe modelos dentro do Claude Code via `/model`, alimentado por
`GET /v1/models`.

- **Automático:** cada modelo habilitado do OpenCode ganha um alias `claude-<provider>-<model>`
  (o prefixo `claude-` é obrigatório — a descoberta do Claude Code só mantém ids contendo
  `claude`/`anthropic`). Linhas com spelling de família first-party (`claude-sonnet-*`,
  `claude-opus-*`, mais `haiku`/`fable`/`mythos`) são blindadas por padrão
  (`cli_shield_aliases = true`): `claude-github-copilot-claude-sonnet-5` é anunciado como
  `claude-github-copilot-cs-5`, para as chamadas de fundo do CLI não caírem no Copilot —
  ver [CLI](config-cli.md#blindando-o-copilot-contra-chamadas-de-fundo-ids-blindados).
  Modelos >= 1M são anunciados como `claude-...[1m]` (único sufixo que
  o Claude lê); use essa forma com sufixo no `settings.json: model` para não aparecer como
  `Custom model` (sem sufixo funciona na API, o gateway remove ao resolver). Linhas com mesmo
  `provider/model` mas `id` distinto (ex. `opus-4.8` vs `opus-4.8-fast`) ganham aliases distintos.
  O catálogo é lido no boot com retry (veja [Erros](erros.md) para o caso `models:0`):
  para refletir modelos novos/removidos depois disso, reinicie o gateway (`--refresh` só
  pré-visualiza o que o boot carregaria). Para o picker do Claude Desktop (que descarta ids
  com nomes de modelos third-party), sete `desktop_aliases = true` — só o id anunciado muda
  (`...-deepseek-...` vira `...-d-eepseek-...`), refs e resolução intactos.
- **Manual:** `[aliases."<gateway-id>"]` no `config.toml` tem precedência sobre os automáticos;
  `[disabled]` esconde refs do picker **sem apagar a linha do catálogo** —
  o alias some do `/v1/models` e nunca é escolhido como default, mas a ref
  direta (`provider/model`) continua resolvendo, então dá para usar no chat
  explicitamente mesmo desabilitado. Com alias desabilitado, use a ref direta
  (`"model": "github-copilot/claude-haiku-4.5"`), não o alias antigo (vira 404).
- `POST /v1/messages` também aceita refs diretas (`opencode-go/kimi-k2.7-code`) e
  model ids simples, mesmo fora da lista.
- **Sem `"model"` na requisição** (probes/chamadas auxiliares): o gateway usa o
  `default_model` do config (ver [Modelo padrão](#modelo-padrão-default_model)), nunca
  "traduz" um modelo em outro — o `gateway_model` no log é sempre o id que o cliente pediu.

### Variantes (`#variant`)

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

## Veja também

- [Configuração no Claude Code CLI](config-cli.md) — env, `settings.json`, subagentes.
- [Configuração no Claude Desktop](config-desktop.md) — managed-settings, `[tiers]`.
- [Erros e diagnóstico](erros.md) — saúde do gateway e tabela de sintomas.
