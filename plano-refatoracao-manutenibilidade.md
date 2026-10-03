# Plano de refatoração — frank-opencode (manutenibilidade)

Escopo: refatoração interna, sem mudar comportamento externo. Rotas, status HTTP,
formato SSE/JSON, resolução de modelos, CLI e envs (`FRANK_PORT`, `FRANK_AUTH_TOKEN`,
`FRANK_CONFIG`) ficam intactos.

Trava de toolchain ao fim de cada fase:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Fase 0 — Trava de comportamento (risco muito baixo)

Objetivo: garantir que qualquer mudança acidental de comportamento seja detectada.
Criar `tests/characterization.rs` (sem tocar `src/`) cobrindo: matriz
protocolo × variante × `body`/`headers` dos forwards não-streaming; sequência SSE
byte-a-byte dos dois tradutores (texto, `tool_use`, `max_tokens`,
`response.failed`, `usage`); colisões de `auto_aliases_for` (`v4.1` vs `v4-1`,
linha `fast`, `evade` on/off, ordenação); tabela
`protocol_for` × `protocol_for_entry` × `is_known_package`; round-trip
`strip_window_suffix`/`window_suffix`; merge de `anthropic-beta` e
`session_headers`/`entry_headers`. Não corrigir bugs achados aqui, só registrar.

## Fase 1 — Quick wins (risco baixo, benefício alto)

Sem mover funções entre arquivos, só extrair helpers locais:

1. `new_message_id(prefix)` único para os 4 pontos de `msg_id` + `mock_message`,
   preservando o formato `msg_<24 hex>`.
2. `find_by_provider_id(catalog, provider, id)` para eliminar os blocos duplicados
   em `lookup_entry`, mantendo ordem de precedência e o `sort + warn` do id simples.
3. Permissão 0600 num único helper (`write_private` passa a cobrir o log do `enable`).
4. `evade_desktop_blocklist`: `to_lowercase` uma vez por token e tabela de tokens
   pré-ordenada (sem mudar a regra de inserção do `-`).
5. `pid_alive` com refresh só do pid alvo; `gateway_healthy` parseando a
   status-line em vez de `contains("200")`.
6. `FRANK_PORT` inválido emite `warn` em vez de silêncio; `endpoint` aisdk
   desconhecido mantém fallback Chat mas com `warn` (provider, pacote, endpoint).
7. Alinhar documentação de `is_known_package` (`contains`) vs `protocol_for`
   (igualdade exata) sem mudar roteamento — a unificação real é Fase 3, com testes.

## Fase 2 — Erros de domínio tipados (risco baixo-médio)

Criar `src/domain/error.rs` com `enum GatewayError` (variantes: `CatalogFetch`,
`CatalogTimeout`, `CatalogShape`, `UnknownModel`, `UnknownVariant`, `NoCredential`,
`NoBaseUrl`, `UpstreamUnreachable`, `UpstreamError`, `InvalidUpstreamJson`,
`BadRequest`) e uma única função de mapeamento para a resposta Anthropic,
reproduzindo exatamente status, `type` e mensagem atuais. Migrar `fetch_catalog`,
`refresh`, `resolve`/`finish_resolve` de `Result<_, String>` para o tipo novo.
Em `config.rs`, `FRANK_PORT` inválido loga e mantém a porta; timeouts zerados
recebem `warn` com clamp documentado. Cobrir o mapeamento erro→HTTP com testes.

## Fase 3 — Dividir `domain/mod.rs` (risco médio)

Quebrar em `model.rs` (ModelRef, CatalogEntry e refs), `protocol.rs` (tabela única
protocolo + known, preservando roteamento byte-idêntico validado pela matriz da
Fase 0), `alias.rs` (AliasEntry, `auto_alias`, `auto_aliases_for`), `slug.rs` e
`desktop.rs` (blocklist + evasão). `mod.rs` vira só re-exports para não quebrar
`crate::domain::*`. Reescrever a cascata de candidatos de `auto_aliases_for` como
iterador explícito `base → id_based → base-2 → id_based-2 → …` com limite nomeado
`MAX_ALIAS_SUFFIX = 100` e `warn` ao esgotar (hoje é `break` silencioso). Cada
arquivo novo leva seus testes junto.

## Fase 4 — Dividir `api/server.rs` (risco médio, maior benefício)

Extrair `state.rs` (AppState + `effective_default`), `catalog.rs` (`refresh`
decomposto em `fetch → log_unknown_packages → build_manual → build_auto →
dedup_e_store`, timeout 20s em const nomeada), `resolve.rs` (`resolve`,
`lookup_entry`, `finish_resolve`), `headers.rs` (session/entry headers,
`apply_entry_body`, fallback session), `errors.rs` (ponte GatewayError→resposta,
`log_upstream_error`, `request_summary`, `body_preview`) e `mock.rs`
(classifier). `routes.rs` fica com router, middleware e handlers finos
(`messages`/`count_tokens` só orquestram: resolver → mock → bearer/base →
despachar). Manter ordem de validação e todas as consts
(`BOOT_CATALOG_*`, `HEARTBEAT_IDLE`, `MAX_REQUEST_BODY`). Meta: `server.rs`
abaixo de 200 linhas ou só re-exports. Os `forward_*` mudam de casa só na Fase 5.

## Fase 5 — Dividir `infra/upstream.rs` + unificar forwards (risco médio-alto)

Criar `infra/upstream/chat.rs` e `responses.rs` (traduções + respectivo tradutor
SSE), `request.rs` (builder único: url, bearer, entry/session headers, merge
`anthropic-beta`, `apply_variant` + `apply_entry_body`, send, status/log/parse —
única divergência preservada: merge de beta no caminho Anthropic e a url),
`sse_pump.rs` (bomba SSE comum parametrizando as 2 diferenças reais: campos de
`usage` e de `finish_reason`), `tokens.rs` (estimate, floor, helpers de bloco) e
`api/forward.rs` (decide protocolo + streaming vs JSON; tradução fica na infra).
Não mudar corpos traduzidos, nomes de campo, floor 16, formato `ping` nem
`HEARTBEAT_IDLE`. Zerar o `#[allow(clippy::too_many_arguments)]` com struct de
parâmetros em vez de `allow`. Os testes byte-a-byte da Fase 0 devem passar
intocados.

## Fase 6 — Tipagem na fronteira (risco alto, FUTURO, não executado)

> Decisão (2026-10-03): fase adiada. É a mais invasiva (toca todos os
> caminhos de handler) e o sandbox de execução não tem toolchain Rust,
> então a migração não seria verificável aqui. As Fases 0-5+7 entregam
> 90% do ganho de manutenibilidade com risco baixo-médio. Quando for
> executar, seguir o desenho abaixo com `cargo test` verde a cada passo.

Definir structs só para a entrada (`MessagesRequest`, `CountTokensRequest`,
`ContentBlock`) com `deny_unknown_fields = false` para não rejeitar nada que hoje
passa. Handlers validam via tipos e convertem para o `Value` interno numa única
`to_value()`; tradutores continuam em `Value` nesta fase. Migrar
`estimate_tokens`, `request_summary` e `system_text` para as structs e testar que
os mesmos inputs geram os mesmos 400/404.

## Fase 7 — Endurecer `daemon.rs` + `infra/opencode.rs` (isolado, a qualquer altura após Fase 1)

Reutilizar `write_private` para pid/port/log, `wait_for(predicado, tentativas,
intervalo)` comum a `enable`/`disable`, `open_ro_conn` único no
`CredentialStore`, `run_opencode_api` único para `fetch_catalog` +
`resolve_db_path`, e documentar a ordem de resolução do DB. Sem mudar paths,
permissões, mensagens de CLI, envelope de credencial (`access` > `key` > raw)
nem filtro `enabled`.

## Riscos transversais

- Mudança acidental de wire: mitigada pela Fase 0; se um teste de
  caracterização falhar, revert.
- Quebrar `clippy -D warnings` no meio: rodar os 3 comandos a cada sub-passo.
- Ciclos de import após o split: `domain` não importa `api`/`infra`; `infra`
  importa `domain`; `api` importa ambos. Se surgir ciclo, o desenho está errado.
- Entrada real que atinja `n > 100` colisões deve gerar `warn` após a Fase 3;
  adicionar regressão com 101 colisões sintéticas.

## Ordem de execução sugerida

Fase 0 → Fase 1 → Fase 2 → Fase 3 → Fase 4 → Fase 5 → Fase 6, com Fase 7 em
paralelo quando conveniente. Commits pequenos, um por fase ou sub-fase, sempre
com os 3 comandos verdes.
