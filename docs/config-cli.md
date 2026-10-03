# Configuração no Claude Code CLI

[← README](../README.md) · [Configuração](configuracao.md) · **CLI** ·
[Desktop](config-desktop.md) · [Erros](erros.md) · [Dev](dev.md) ·
[Arquitetura](arquitetura.md)

Como apontar o **Claude Code** (CLI) para o gateway: variáveis de ambiente,
`~/.claude/settings.json`, subagentes, janela de contexto e a blindagem contra
chamadas de fundo indesejadas. As opções do lado do gateway estão em
[Configuração](configuracao.md).

## Variáveis de ambiente

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3737
export ANTHROPIC_AUTH_TOKEN=dummy
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1  # habilita entradas no picker /model
claude
```

`ANTHROPIC_AUTH_TOKEN` deve ser igual ao `auth_token` do gateway (qualquer valor
se o gateway estiver com `auth_token = ""`).

Persistindo em `~/.claude/settings.json` (escopo do usuário, nunca no arquivo
compartilhado do projeto):

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3737",
    "ANTHROPIC_AUTH_TOKEN": "dummy",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
  }
}
```

O picker `/model` é alimentado por `GET /v1/models`; a semântica de aliases,
`[aliases]`/`[disabled]`, refs diretas e variantes está em
[Configuração → Escolhendo modelos](configuracao.md#escolhendo-modelos--modelmap).
Resumo: ids precisam conter `claude`/`anthropic` (os aliases automáticos já
contêm), janelas >= 1M aparecem como `claude-...[1m]` e o `--refresh` só
pré-visualiza o catálogo (reinicie o gateway para recarregar).

## Fixando o modelo dos subagentes

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

## Blindando o Copilot contra chamadas de fundo (ids blindados)

**Sintoma:** `WARN upstream rejected request … status=429 body=quota exceeded`
com `gateway_model=claude-github-copilot-claude-sonnet-5`, `stream=true`,
`max_tokens: 64000` e `tools: []`, sem você ter selecionado o Copilot.

**Causa:** o CLI do Claude Code (ao contrário do Desktop) resolve os modelos
de fundo (small-fast e fallbacks de família haiku→sonnet→opus→fable→mythos)
**canonicando os ids descobertos por substring** — `claude-sonnet-5` /
`claude-opus-*` dentro do id. As linhas do Copilot são as primeiras em ordem
alfabética e casam; os tiers não entram nessa conta (o CLI os descarta — veja
a nota em [Configuração do Desktop](config-desktop.md#tiers-da-família-anthropic)).
O `max_tokens: 64000` é o `max_output_tokens` default do `claude-sonnet-5` no
catálogo embutido do CLI; `stream=true` + `tools: []` identifica uma *side
query* (title-gen etc.), não o seu turno principal — esse continua indo para o
`default_model` com tools e funciona normalmente.

**Correção (automática, padrão):** `cli_shield_aliases = true` (default) faz o
gateway reescrever os fragmentos com spelling de família antes do slug, como o
`desktop_aliases` faz para a denylist — sem `[aliases]` manual, então novos
providers que tragam modelos `claude-*` já nascem blindados. Os ids anunciados
usam `cs` (copilot-sonnet) / `co` (copilot-opus) / `ch` / `cf` / `cm`;
`display_name` e ref ficam iguais, então o picker continua legível e a seleção
intencional continua funcionando:

| id histórico (some do `/v1/models`) | id blindado | ref (continua resolvendo) |
|---|---|---|
| `claude-github-copilot-claude-sonnet-5` | `claude-github-copilot-cs-5` | `github-copilot/claude-sonnet-5` |
| `claude-github-copilot-claude-sonnet-5-5` | `claude-github-copilot-cs-5-5` | `github-copilot/claude-sonnet-5.5` |
| `claude-github-copilot-claude-opus-4-8` | `claude-github-copilot-co-4-8` | `github-copilot/claude-opus-4.8` |
| `claude-github-copilot-claude-opus-4-8-fast` | `claude-github-copilot-co-4-8-fast` | `github-copilot/claude-opus-4.8-fast` |
| `claude-github-copilot-claude-opus-5` | `claude-github-copilot-co-5` | `github-copilot/claude-opus-5` |
| `claude-github-copilot-claude-opus-5-5` | `claude-github-copilot-co-5-5` | `github-copilot/claude-opus-5.5` |

Efeitos:

- os ids históricos somem do `/v1/models` (se um `settings.json: model` antigo
  referenciar um deles, vira 404 — atualize para o id novo ou para a ref);
- a **ref direta continua resolvendo** em `POST /v1/messages`
  (`"model": "github-copilot/claude-sonnet-5"`) — dá para usar o Copilot de
  propósito sem mudar nada quando a quota voltar;
- nenhum id descoberto casa mais o spelling de família, então as chamadas de
  fundo do CLI param de parar no Copilot;
- `[tiers]` continua valendo: prefira chavear pela ref (`provider/model`),
  que é estável entre renomeações — a chave por gateway id também funciona,
  mas precisa acompanhar o id blindado.

**Opt-out:** `cli_shield_aliases = false` mantém o spelling histórico
`claude-<provider>-<model>` (útil se algum fluxo externo depende dos ids
antigos). Para blindar só linhas específicas nesse modo, use `[aliases]`
manual por linha.

**Complemento (opção C):** fixe o small-fast do CLI no modelo barato, para
title-gen e afins nunca consultarem a família por fallback. No bloco `env` do
`~/.claude/settings.json`:

```json
"ANTHROPIC_SMALL_FAST_MODEL": "claude-opencode-go-muse-spark-1-3-contributor[1m]"
```

**Observabilidade:** o `WARN upstream rejected request` do gateway loga
também `user_agent=` e `session=` (`x-claude-code-session-id`). Depois de
reiniciar, qualquer WARN novo identifica o cliente/fluxo na hora — se ainda
sobrar chamada indesejada, esses dois campos dizem de onde ela veio.

Verificação depois do restart:

```bash
curl -s http://127.0.0.1:3737/v1/models | jq -r '.data[].id' | grep copilot
# deve listar só ...-cs-* / ...-co-* e os gpt/grok/gemini (evadidos), nenhum claude-sonnet/claude-opus
```

Para reverter: sete `cli_shield_aliases = false` e apague o
`ANTHROPIC_SMALL_FAST_MODEL` do settings (backups em `*.bak` ao lado dos
arquivos) e reinicie.

## Janela de contexto manual (quando o auto-sufixo não basta)

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

## Verificação antes de abrir o Claude Code

```bash
# 0. Catálogo carregado? status "ok" E "models" != 0 (veja docs/erros.md se não)
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

## Veja também

- [Configuração do gateway](configuracao.md) — `config.toml`, aliases, variantes.
- [Configuração no Claude Desktop](config-desktop.md) — o app Desktop.
- [Erros e diagnóstico](erros.md) — sintomas comuns (401, 429, `models:0`, etc.).
