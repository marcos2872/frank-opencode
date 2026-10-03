#!/usr/bin/env bash
# Publica um comentário sticky de resumo na PR atual.
#
#   pr-sticky-comment.sh <marker> <body-file>
#
# Lê do ambiente: PR_NUMBER (vazio em push → no-op), GH_TOKEN,
# GITHUB_REPOSITORY, GITHUB_SERVER_URL, GITHUB_RUN_ID.
#
# O <marker> (ex.: `<!-- frank-ci-summary -->`) identifica o comentário no
# repo: se já existe um com esse marcador, ele é atualizado (PATCH) em vez de
# acumular um por push. Marcadores diferentes convivem (um por job). Falhas do
# `gh` viram warning — em PR de fork o token é read-only e o comentário é
# pulado sem derrubar o job.
set -u

marker="$1"
body_file="$2"

# Push (sem PR): nada a publicar.
if [ -z "${PR_NUMBER:-}" ]; then
  exit 0
fi

comment=$(mktemp)
trap 'rm -f "$comment"' EXIT
{
  echo "$marker"
  cat "$body_file"
  echo ""
  echo "📊 [Ver run completo](${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID})"
  echo ""
} > "$comment"

existing=$(gh api --paginate "repos/${GITHUB_REPOSITORY}/issues/${PR_NUMBER}/comments" \
  --jq ".[] | select(.body | contains(\"${marker}\")) | .id" 2>/dev/null | head -n1 || true)

if [ -n "$existing" ]; then
  gh api -X PATCH "repos/${GITHUB_REPOSITORY}/issues/comments/${existing}" \
    -F "body=@${comment}" \
    || echo "::warning::não atualizou o comentário ${marker} no PR #${PR_NUMBER}"
else
  gh api -X POST "repos/${GITHUB_REPOSITORY}/issues/${PR_NUMBER}/comments" \
    -F "body=@${comment}" \
    || echo "::warning::não criou o comentário ${marker} no PR #${PR_NUMBER}"
fi
exit 0
