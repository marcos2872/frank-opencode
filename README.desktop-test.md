# frank-opencode no Claude Desktop — ponto de atenção (NÃO TESTADO)

> Status: pesquisa documental apenas. Não testado em máquina real.
> Não commitar este arquivo até a validação ser feita.

## Resposta curta

Provavelmente funciona, mas por outro caminho de configuração que o CLI.
O app Desktop ignora `ANTHROPIC_BASE_URL` e `~/.claude/settings.json`.

## Como configurar (quando for testar)

1. Subir o daemon: `frank-opencode --enable` (ex.: `http://127.0.0.1:3737`).
2. No Claude Desktop: `Help → Troubleshooting → Enable Developer Mode`
   (reinicia com menu Developer).
3. `Developer → Configure Third-Party Inference`:
   - Connection / Inference provider: `Gateway`
   - Gateway base URL: `http://127.0.0.1:<porta>` (sem sufixo `/v1`;
     o app anexa `/v1/messages` sozinho; usar `127.0.0.1`, não `localhost`)
   - Credential kind: `Static API key`
   - Gateway API key: valor do `auth_token` (ou qualquer placeholder se vazio)
   - Gateway auth scheme: `Bearer` ou `x-api-key` (ambos aceitos)
4. `Apply locally → Relaunch`, sair da conta Anthropic,
   `Continue with Gateway`.

Refs: `https://claude.com/docs/third-party/claude-desktop/gateway`,
`https://code.claude.com/docs/en/llm-gateway-connect`.

## Por que deve funcionar

- Desktop exige `POST /v1/messages` com streaming + tool use (obrigatório)
  e `GET /v1/models` (opcional) — o frank serve os dois.
- HTTP em loopback é permitido (só host não-loopback exige HTTPS).
- Aliases `claude-*` passam no filtro de descoberta ("só ids
  reconhecivelmente Claude").
- `ping` SSE a cada 20s no frank alimenta o watchdog do Desktop
  (`inferenceStreamIdleTimeoutSec`).

## Pontos de atenção para o teste

1. [ ] Aliases aparecem no picker de modelos?
2. [ ] Chat simples responde via kimi/minimax?
3. [ ] Aba Code do Desktop funciona (ela usa o mesmo gateway)?
4. [ ] Modelos não-Anthropic atrás de alias `claude-*` — há relatos
   conflitantes (Opper diz que Cowork só aceita família Claude;
   Eigent/OpenRouter diz que qualquer um passa).
5. [ ] `cache_control`/betas experimentais: o frank traduz para
   Chat/Responses no upstream, então prompt caching e tool search
   podem não ter pass-through total (custo maior ou `400` em betas).
6. [ ] Se o picker vier vazio ou sem tier `sonnet`/`opus`, avaliar
   expor em `/v1/models`: `display_name`, `anthropic_family_tier`,
   `supports1m`.
7. [ ] Desktop desatualizado ou plano Team pode ocultar o menu
   Third-Party Inference mesmo com Developer Mode.
8. [ ] Cowork agents com acesso web podem exigir liberar
   `Allowed egress hosts`.

## Verificação rápida antes de abrir o app

```bash
curl -s -X POST http://127.0.0.1:3737/v1/messages \
  -H "Authorization: Bearer $TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-opencode-go-kimi-k2-7-code","max_tokens":1,
       "messages":[{"role":"user","content":"."}]}'
# 401 = credencial errada; erro de modelo desconhecido ainda prova
# que URL + credencial estão OK.
```
