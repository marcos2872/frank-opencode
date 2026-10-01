# frank-opencode no Claude Desktop

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

## Diagnóstico

```bash
grep -E "managed-settings|gateway|discovery|3p" ~/.config/Claude/logs/main.log | tail -n 20
# EACCES no managed-settings.json = permissão (precisa 0644 root:root);
# rejeição de chave = nome/valor inválido (confere a referência oficial)
```

## Por que só alguns modelos aparecem no picker

O Desktop valida cada id descoberto no código (`Ro("gateway", id)`): o id
precisa conter `claude`/`anthropic`/família **e** não conter nenhum token da
denylist embutida (`deepseek`, `kimi`, `glm`, `gpt`, `grok`, `qwen`,
`gemini`, `minimax`, `longcat`, `mimo`, `hy3`, ...). Os aliases padrão
`claude-<provider>-<model>` carregam o nome upstream no id, então os 40
dessa lista caem — sobram os 11 cujos nomes escapam (`opus`, `sonnet`,
`mai-code`, `hy4`, `muse-spark`, `space-bunny`). Listas explícitas
(`inferenceModels`) passam pelo mesmo filtro, então não adianta listar lá.

Para listar tudo, ative a evasão no frank (`config.toml` ou
`FRANK_CONFIG` apontando para um arquivo com):

```toml
desktop_aliases = true
```

e reinicie o gateway + o Desktop. Só o id anunciado muda
(`deepseek` → `d-eepseek`); refs, display names e resolução intactos.
Trade-off: se a Anthropic ampliar a denylist, novos tokens podem cair de
novo — o log `Model discovery: N found; picker = M` denuncia na hora.

## Por que deve funcionar

- Desktop exige `POST /v1/messages` com streaming + tool use (obrigatório)
  e `GET /v1/models` (opcional, alimenta o picker via `modelDiscoveryEnabled`)
  — o frank serve os dois.
- HTTP em loopback é permitido (só host não-loopback exige HTTPS).
- Aliases `claude-*` passam no filtro de descoberta — com `desktop_aliases`
  até os nomes bloqueados (`deepseek`, `kimi`, ...) listam (validados 51/51).
- `ping` SSE a cada 20s no frank alimenta o watchdog do Desktop
  (`inferenceStreamIdleTimeoutSec`).

## Pontos de atenção para o teste

1. [x] `Continue with Gateway` aparece no login (exige `0644`, `0600` dá `EACCES`).
2. [x] Aliases aparecem no picker de modelos (51/51 com `desktop_aliases`).
3. [ ] Chat simples responde via kimi/minimax?
4. [ ] Aba Code do Desktop funciona (ela usa o mesmo gateway)?
5. [ ] Modelos não-Anthropic atrás de alias `claude-*` — há relatos
   conflitantes (Opper diz que Cowork só aceita família Claude;
   Eigent/OpenRouter diz que qualquer um passa).
6. [ ] `cache_control`/betas experimentais: o frank traduz para
   Chat/Responses no upstream, então prompt caching e tool search
   podem não ter pass-through total (custo maior ou `400` em betas).
7. [ ] Se o picker vier vazio ou sem tier `sonnet`/`opus`, avaliar
   expor em `/v1/models`: `display_name`, `anthropic_family_tier`,
   `supports1m`.
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
