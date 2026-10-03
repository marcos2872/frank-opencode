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

Pré-requisitos:

- **OpenCode v2** instalado e logado (`opencode auth login`) — é quem autentica no Console e fornece o catálogo de modelos.
- `curl` e (opcionalmente) o `gh` CLI para baixar o binário.

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

> Caminho alternativo: `cargo install --path .` ou
> `cargo install --git https://github.com/marcos2872/frank-opencode` (instalação
> a partir do código — veja [Desenvolvimento](docs/dev.md)).

Depois de instalar, crie o `~/.config/frank-opencode/config.toml` e aponte seu
cliente para o gateway:

- **Claude Code (CLI):** [Configuração no CLI](docs/config-cli.md)
- **Claude Desktop:** [Configuração no Desktop](docs/config-desktop.md)

## Segurança

- Só escuta em `127.0.0.1`. DB aberto em `READ_ONLY`. Segredos ficam só em memória (`secrecy`), nunca logados.
- Arquivos de estado (`frank.pid`, `frank.port`, `frank.session`, `frank.log`) criados com `0600`.
- Quando `auth_token` está setado, todo endpoint exceto `/health` o exige
  (`x-api-key` ou `Authorization: Bearer`) — veja
  [Configuração → auth_token](docs/configuracao.md#autenticação-do-gateway-auth_token).

## Desenvolvimento

CI (GitHub Actions) roda formatação, lint e a suíte completa em todo push e PR.
A branch `main` está protegida: o status **test** é obrigatório e a branch precisa
estar atualizada — uma PR só mergeia com os testes verdes. Há também um hook de
pre-commit opcional. Como rodar em dev, testes, release e a suíte de latência:
[Desenvolvimento](docs/dev.md).

## Documentação

- [Configuração do gateway](docs/configuracao.md) — `config.toml`, auth, `default_model`, aliases, variantes.
- [Configuração no Claude Code CLI](docs/config-cli.md) — env, `settings.json`, subagentes, blindagem do Copilot.
- [Configuração no Claude Desktop](docs/config-desktop.md) — managed-settings, `desktop_aliases`, `[tiers]`.
- [Erros e diagnóstico](docs/erros.md) — `/health`, logs e tabela de sintomas.
- [Desenvolvimento](docs/dev.md) — como rodar em dev, testes, lint, releases.
- [Arquitetura](docs/arquitetura.md) — como o gateway é estruturado (domínio, infra, API).
