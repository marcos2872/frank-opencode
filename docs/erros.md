# Erros e diagnóstico

[← README](../README.md) · [Configuração](configuracao.md) · [CLI](config-cli.md) ·
[Desktop](config-desktop.md) · **Erros** · [Dev](dev.md) ·
[Arquitetura](arquitetura.md)

Como checar a saúde do gateway, onde ficam os logs, a tabela de sintomas mais
comuns e os avisos que são esperados (não são erro).

## Saúde e diagnóstico

`GET /health` (sem auth) reporta `ok` / `degraded` / `starting`, a contagem de modelos,
o modelo padrão efetivo e o `last_error` do load do catálogo no boot. No boot o catálogo
é tentado até 6 vezes com backoff crescente (~2s → 16s) enquanto vier vazio (comum quando
`opencode api` (re)inicia o serviço); esgotadas as tentativas com 0 modelos, `last_error`
fica preenchido e o status vira `degraded`, em vez de parecer `ok` com catálogo vazio.
Não há refresh automático depois do boot — para atualizar o catálogo, reinicie o gateway.

```bash
curl -s http://127.0.0.1:3737/health | jq '{status, models, default_model, last_error}'
```

Logs:

- Daemon: `~/.local/share/frank-opencode/frank.log`.
- Foreground (`--serve`): saída do próprio processo (`RUST_LOG` controla o nível —
  veja [Dev](dev.md#rodando)).

O `frank.log` é só-append: trunque de vez em quando (`: > frank.log`).

## Tabela de sintomas

| Sintoma | Causa / correção |
|---|---|
| `401` "no stored credential for 'X'" | Rode `opencode auth login` para aquele provider. |
| `400` "IDE authentication failed ... invalid token: unknown format" | Provider `github-copilot`: a credencial é envelope OAuth (`access`), não API-key (`key`). O gateway extrai `access`; se o erro persistir, re-autentique: `opencode auth login` e `opencode auth switch` (ajuste o `switch` para o provider copilot). |
| `400` "`X` is not accessible via the /chat/completions endpoint" | Provider `github-copilot`: GPT-6/5.6, grok, mai-code e codex são servidos pela Responses API, não Chat Completions. O gateway roteia pelo `settings.endpoint` que o catálogo declara por modelo (`"responses"`/`"chat"`/`"messages"`); reinicie o gateway para pegar o binário novo. |
| `401` "invalid gateway credential" | `auth_token` está setado no config: `ANTHROPIC_AUTH_TOKEN` precisa ser igual (veja [Configuração → auth_token](configuracao.md#autenticação-do-gateway-auth_token)). |
| `MissingSessionID` do Go | frank-opencode < 0.1.1; atualize (headers de sessão agora são automáticos). |
| `FreeTierError` em modelos `opencode/*` | Free tier do Console só funciona dentro do OpenCode; esses modelos ficam ocultos de `/v1/models` por padrão (`include_free_tier = true` para exibir). Use um modelo `opencode-go/*`. |
| Saída vazia com `max_tokens` minúsculo | Modelos de reasoning gastam o orçamento no reasoning primeiro; aumente `max_tokens` (o Claude Code já faz isso por padrão). |
| Modelos faltando no `/model` | Sete `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`; ids precisam conter `claude`/`anthropic` (os aliases automáticos já contêm). |
| `/health` mostra `"models":0` e `/v1/models` vazio | O catálogo é lido no boot quando o serviço/backend do OpenCode ainda não estava pronto: `opencode api get /api/model` pode (re)iniciar o serviço, e o primeiro fetch volta com catálogo vazio sem registrar erro (`last_error:null`). O gateway tenta até 6 vezes com backoff e marca `degraded` quando esgota; se seguir vazio, verifique `opencode service status` e reinicie com `frank-opencode --disable && frank-opencode --enable`. |
| `400` citando `context_management`/`output_config` | O upstream rejeita um campo pré-release; tente com `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`. |
| `"X" isn't described by this version's model catalog` | Esperado: ids do gateway são sintéticos e o Claude Code assume janela de 200k. O gateway anuncia a janela real do catálogo do OpenCode (`limit.context`) em `/v1/models` — como campo `context_window` e, para janelas >= 1M, como sufixo `[1m]` no id. Valide com `curl -s http://127.0.0.1:3737/v1/models \| jq '.data[] \| {id, context_window}'`. Janelas < 1M: use `modelOverrides` ([Configuração CLI](config-cli.md#janela-de-contexto-manual-quando-o-auto-sufixo-não-basta)). |
| `Waiting for API response · will retry` (travamentos) | Pausas longas de reasoning sem bytes no stream. O gateway injeta frames `ping` durante o silêncio do upstream; se persistir, cheque `frank.log` por erros do upstream e considere aumentar `API_TIMEOUT_MS`. |
| Modelo aparece como `Custom model` no `/model` | O picker do Claude Code faz match exato do `id`. Para janelas >= 1M o gateway anuncia `claude-...[1m]`; se seu `settings.json: model` tem a forma sem sufixo (ou vice-versa), funciona na API (o gateway remove o sufixo ao resolver) mas aparece como custom. Reselecione a forma com `[1m]` no `/model`. |
| `400 {"model":"X"}` intermitente | Passthrough do upstream Go (ex. indisponibilidade pontual, limite, modelo em rolagem). O gateway loga em `frank.log` com `gateway_model/opencode_ref/base_url/status/body` para diagnóstico. Trocar de modelo e voltar costuma resolver; se persistir, reinicie o gateway. |
| WARN `upstream rejected request` `429 quota exceeded` em `claude-sonnet-5` sem você selecionar esse modelo | Três origens distintas, pelo shape do `req` no log: (1) probes/classificador do auto-mode — sem `stream`, `max_tokens` ≤ 4 ou system com `<block>`/`<severity>` — ligue `mock_classifier = true` ([Configuração](configuracao.md#mock-do-classificador-do-auto-mode-mock_classifier)); (2) títulos do **Claude Desktop** — `stream` ausente, `max_tokens: 200`, sem `tools` — use `[tiers]` ([Configuração Desktop](config-desktop.md#tiers-da-família-anthropic)); (3) side queries do **CLI do Claude Code** — `stream=true`, `max_tokens: 64000`, `tools: []` — o CLI ignora os tiers e casa a família por substring no id: o shield automático (`cli_shield_aliases`, default on) reescreve o spelling de família nos ids anunciados ([Configuração CLI](config-cli.md#blindando-o-copilot-contra-chamadas-de-fundo-ids-blindados)). O WARN loga `user_agent=`/`session=` para distinguir as origens. |
| WARN `upstream rejected request` `400 model_not_supported` num modelo (ex. Copilot) que você não selecionou | Chamada auxiliar sem `"model"` caiu no fallback = primeiro alias alfabético. O `gateway_model` no log é sempre o id que o cliente pediu — o gateway nunca "traduz" um modelo em outro. Fixe `default_model` no config ([Modelo padrão](configuracao.md#modelo-padrão-default_model)). |
| `400` "`max_output_tokens` The number must be `>= 16`" ao trocar de modelo no meio do chat | O Claude Code verifica o modelo antes de trocar com um probe `max_tokens: 1`; o backend zen rejeita limites de saída abaixo de 16 em alguns modelos (ex. muse-spark). O gateway eleva `max_tokens` < 16 para 16 na tradução — só afeta sondagens (requisições reais usam milhares de tokens). Em sessão vazia não há probe, por isso a troca ali sempre funcionou. |
| `400` "`The request contains invalid parameters`" ao fazer `/compact` (sessão longa com tools) | O backend opencode-go rejeitava texto de usuário (ex.: injeção "The user sent a new message while you were working") entre `function_call`s pendentes e seus `function_call_output` no corpo traduzido para Responses. O tradutor agora adia esses itens do usuário para depois dos outputs (`infra/upstream/responses.rs`) — atualize o binário. Para diagnosticar rejects do Responses, use o dump do corpo traduzido ([Dev](dev.md#dump-do-corpo-traduzido-responses)). |
| Modelo removido/renomeado no `opencode-go` continua listado | Catálogo é lido só no boot: após `opencode auth` novo ou rolagem de modelos (`opus-4.7` → `opus-4.8`), rode `frank-opencode --disable && frank-opencode --enable`. |
| `Claude Opus 4.8` vs `Claude Opus 4.8 Fast` | Mesmo `modelID`, `id`/`headers` diferentes (`fast-mode-2026-02-01` + `{"speed":"fast"}`). O gateway gera 2 aliases distintos (`...-opus-4-8` e `...-opus-4-8-fast`) e repassa `anthropic-beta`/`speed` do catálogo. |

> **Contexto:** o Claude Code pode mostrar *"There's an issue with the selected model
> (claude-…)"* de forma intermitente mesmo com o gateway saudável. Essa mensagem é genérica —
> qualquer 4xx do upstream cujo texto cite o modelo a dispara (indisponibilidade pontual ou
> limite de uso do provedor, ex. `opencode.ai/zen/go`). Ela **não** significa que o alias sumiu
> do catálogo: como o catálogo é lido só no boot, `/v1/models` e `/health` continuam normais
> nesses momentos. Se o modelo realmente sumir do lado do provedor, reinicie o gateway.

## Avisos esperados (não são erro)

### Auto-mode ("session isn't eligible...")

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

## Veja também

- [Configuração do gateway](configuracao.md) — ajustar `config.toml`.
- [Configuração no Claude Code CLI](config-cli.md) — env e `settings.json`.
- [Configuração no Claude Desktop](config-desktop.md) — setup do app Desktop.
- [Desenvolvimento](dev.md) — rodar em dev e reproduzir o CI.
