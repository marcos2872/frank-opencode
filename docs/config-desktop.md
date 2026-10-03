# Configuração no Claude Desktop

[← README](../README.md) · [Configuração](configuracao.md) · [CLI](config-cli.md) ·
**Desktop** · [Erros](erros.md) · [Dev](dev.md) · [Arquitetura](arquitetura.md)

Como apontar o app **Claude Desktop** para o gateway. As opções do lado do
gateway (auth, aliases, variantes) estão em [Configuração](configuracao.md).

Rota Linux validada em máquina real (Fedora, porte comunitário
`claude-desktop-2.16120.0`): managed file + `desktop_aliases` listam os
51 modelos no picker.

## Resposta curta

No Linux (porte comunitário — a Anthropic só distribui Desktop para
Mac/Windows) o menu `Help → Troubleshooting → Enable Developer Mode` pode
não existir. Não precisa dele: no Linux a configuração 3P é um arquivo
managed lido no boot.

## Como configurar no Linux (sem menu)

1. Subir o daemon: `frank-opencode --enable` (ex.: `http://127.0.0.1:3737`).
2. Criar `/etc/claude-desktop/managed-settings.json`:
```json
{
  "inferenceProvider": "gateway",
  "inferenceGatewayBaseUrl": "http://127.0.0.1:3737",
  "inferenceCredentialKind": "static",
  "inferenceGatewayApiKey": "dummy",
  "inferenceGatewayAuthScheme": "bearer",
  "modelDiscoveryEnabled": true
}
```
`inferenceGatewayApiKey` = valor do `auth_token` do gateway (ou qualquer
placeholder se vazio). `bearer` e `x-api-key` são ambos aceitos pelo frank.
3. Permissões são eliminatórias (arquivo regular, `root:root`, sem escrita
   para grupo/outros — vale também para o diretório; se falhar, o app
   ignora o managed **e** desabilita o local). Atenção: `0600` **não**
   funciona — o app roda como seu usuário e precisa conseguir **ler** o
   arquivo (`EACCES` no `main.log`). Use `0644` (root lê/escreve, resto só
   lê: continua satisfazendo "sem escrita para grupo/outros"):
```bash
sudo mkdir -p /etc/claude-desktop
sudo tee /etc/claude-desktop/managed-settings.json > /dev/null <<'EOF'
{
  "inferenceProvider": "gateway",
  "inferenceGatewayBaseUrl": "http://127.0.0.1:3737",
  "inferenceCredentialKind": "static",
  "inferenceGatewayApiKey": "dummy",
  "inferenceGatewayAuthScheme": "bearer",
  "modelDiscoveryEnabled": true
}
EOF
sudo chown root:root /etc/claude-desktop/managed-settings.json
sudo chmod 0644 /etc/claude-desktop/managed-settings.json
sudo chmod 0755 /etc/claude-desktop
```
4. Fechar o Desktop por completo e reabrir (config só é lida no boot).
5. Na tela de login deve aparecer `Continue with Gateway` (sair da conta
   Anthropic se já logado).

Sobre a base URL: começa **sem** sufixo `/v1` (o app anexa `/v1/messages`
sozinho; usar `127.0.0.1`, não `localhost`). Se der 404, olha nos logs do
frank o path que chegou e ajusta.

## Rota alternativa (builds oficiais Mac/Windows)

`Help → Troubleshooting → Enable Developer Mode` (reinicia com menu
Developer), depois `Developer → Configure Third-Party Inference` com os
mesmos valores acima (provider `Gateway`, `Static API key`, scheme
`Bearer`). Em builds recentes o toggle pode estar em avatar → Settings →
Developer Mode.

## Modelos visíveis no picker (`desktop_aliases`)

O Desktop valida cada id descoberto no código (`Ro("gateway", id)`): o id
precisa conter `claude`/`anthropic`/família **e** não conter nenhum token da
denylist embutida (`deepseek`, `kimi`, `glm`, `gpt`, `grok`, `qwen`,
`gemini`, `minimax`, `longcat`, `mimo`, `hy3`, ...). Os aliases padrão
`claude-<provider>-<model>` carregam o nome upstream no id, então os 40
dessa lista caem — sobram os 11 cujos nomes escapam (`opus`, `sonnet`,
`mai-code`, `hy4`, `muse-spark`, `space-bunny`). Listas explícitas
(`inferenceModels`) passam pelo mesmo filtro, então não adianta listar lá.

Para listar tudo, ative a evasão no frank. No
`~/.config/frank-opencode/config.toml` (cria se não existir — o gateway lê no
boot, então reinicie depois):

```toml
port = 3737
auth_token = ""
opencode_bin = "opencode"
include_free_tier = false

# Fallback de requisições sem "model" (sem ele vale o 1º alias alfabético,
# hoje um claude-github-copilot-... — ver "Modelo padrão" em configuracao.md).
default_model = "claude-opencode-go-muse-spark-1-3-contributor"

# Lista todos os modelos no picker do Claude Desktop
# (reescreve deepseek -> d-eepseek etc. só no id anunciado)
desktop_aliases = true
```

e reinicie o gateway + o Desktop. Só o id anunciado muda
(`deepseek` → `d-eepseek`); refs, display names e resolução intactos.
Trade-off: se a Anthropic ampliar a denylist, novos tokens podem cair de
novo — o log `Model discovery: N found; picker = M` denuncia na hora.

Demais opções (auth, timeouts, `mock_classifier`, `[aliases]`, `[disabled]`,
variantes) estão em [Configuração](configuracao.md).

Sobre o fallback: se alguma chamada chegar sem `model`, o gateway usa esse
`default_model` — sem ele, cai no primeiro alias em ordem alfabética (um
modelo do Copilot, que pode nem ter acesso na sua conta e gera `400
model_not_supported` no log sem você ter escolhido o Copilot). `[disabled]`
tira refs do picker; no Desktop o picker é o único jeito de escolher modelo,
então desabilitado = não selecionável ali (a ref direta `provider/model`
continua resolvendo para clientes de API como o Claude Code).

## Tiers da família Anthropic

O Claude Desktop gera o título de cada conversa em background com a classe
`small_fast`: ele escolhe o primeiro modelo descoberto com tier `haiku`, senão
`sonnet`, senão `opus` — e ignora o modelo da sua sessão. Sem tier anunciado,
o Desktop cai num match por substring no id e resolve no primeiro `*sonnet*`
alfabético (hoje a linha do Copilot), queimando a quota dele a cada título
(`WARN upstream rejected request … 429 quota exceeded` com `max_tokens: 200`,
sem `tools`, sem `stream`).

A tabela `[tiers]` anuncia `anthropic_family_tier` (e `is_family_default` para
o vencedor do tier) nos itens do `/v1/models`. A chave casa gateway id ou ref
`provider/model`, como o `[disabled]`:

```toml
[tiers."claude-opencode-go-muse-spark-1-3-contributor"]
tier = "haiku"
family_default = true

[tiers."github-copilot/claude-sonnet-5"]
tier = "sonnet"
```

Tiers válidos: `haiku`, `sonnet`, `opus`, `fable`, `mythos` (qualquer outro
valor é erro de config, como o resto do arquivo). Aliases sem mapeamento não
anunciam tier — comportamento atual, nada quebra. Reinicie o gateway (config
lido no boot) e confira:

```bash
curl -s http://127.0.0.1:3737/v1/models | jq '.data[] | select(.anthropic_family_tier != null) | {id, anthropic_family_tier, is_family_default}'
```

Com o muse-spark como `haiku` + `family_default`, os títulos do Desktop passam
a ir para ele em vez do Copilot — que continua selecionável no picker para o
chat intencional.

> **Atenção — tiers valem só para o Desktop.** O CLI do Claude Code
> **descarta** `anthropic_family_tier` do `/v1/models` (o schema de cache dele
> guarda só `{id, display_name, description}` e janela; o campo camelCase
> `anthropicFamilyTier` que ele conhece é da config `models` do
> managed-settings, não do discovery). Chamadas de fundo do CLI que caem no
> Copilot não são cobertas por `[tiers]` — veja
> [Blindando o Copilot](config-cli.md#blindando-o-copilot-contra-chamadas-de-fundo-ids-renomeados).

## Por que deve funcionar

- Desktop exige `POST /v1/messages` com streaming + tool use (obrigatório)
  e `GET /v1/models` (opcional, alimenta o picker via `modelDiscoveryEnabled`)
  — o frank serve os dois.
- HTTP em loopback é permitido (só host não-loopback exige HTTPS).
- Aliases `claude-*` passam no filtro de descoberta — com `desktop_aliases`
  até os nomes bloqueados (`deepseek`, `kimi`, ...) listam (validados 51/51).
- `ping` SSE a cada 20s no frank alimenta o watchdog do Desktop
  (`inferenceStreamIdleTimeoutSec`).

## Diagnóstico

```bash
grep -E "managed-settings|gateway|discovery|3p" ~/.config/Claude/logs/main.log | tail -n 20
# EACCES no managed-settings.json = permissão (precisa 0644 root:root);
# rejeição de chave = nome/valor inválido (confere a referência oficial)
```

## Pontos de atenção para o teste

1. [x] `Continue with Gateway` aparece no login (exige `0644`, `0600` dá `EACCES`).
2. [x] Aliases aparecem no picker de modelos (51/51 com `desktop_aliases`).
3. [x] Chat simples responde via kimi/minimax.
4. [x] Aba Code do Desktop funciona (ela usa o mesmo gateway).
5. [x] Modelos não-Anthropic atrás de alias `claude-*` funcionam (com `desktop_aliases`).
6. [x] `cache_control`/betas experimentais: o frank traduz para
   Chat/Responses no upstream.
7. [x] Picker com tiers `sonnet`/`opus` via descoberta.
8. [x] Cowork agents com acesso web.

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

## Veja também

- [Configuração do gateway](configuracao.md) — `config.toml`, auth, aliases.
- [Configuração no Claude Code CLI](config-cli.md) — o CLI, que tem armadilhas próprias.
- [Erros e diagnóstico](erros.md) — 429 de título, permissões do managed file.
