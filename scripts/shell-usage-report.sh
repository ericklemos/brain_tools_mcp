#!/usr/bin/env bash
# Summarize .brain/shell-usage.jsonl — which commands brain_shell actually runs.
# Usage: ./scripts/shell-usage-report.sh [path/to/shell-usage.jsonl]
set -eu

file="${1:-.brain/shell-usage.jsonl}"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq não encontrado; instale jq para analisar o log." >&2
  exit 1
fi

if [[ ! -f "$file" ]]; then
  echo "sem log de uso: $file" >&2
  exit 1
fi

valid="$(mktemp)"
trap 'rm -f "$valid"' EXIT
# Skip corrupt lines instead of failing the whole report.
jq -R -c 'fromjson? | select(.)' "$file" > "$valid"

total="$(wc -l < "$valid" | tr -d ' ')"
if [[ "$total" == "0" ]]; then
  echo "log vazio: $file"
  exit 0
fi

ok="$(jq -r 'select(.status == "ok") | 1' "$valid" | wc -l | tr -d ' ')"
echo "# brain_shell usage — $file"
echo "total: $total | ok: $ok | falhas: $((total - ok))"
echo

echo "## programas mais chamados"
jq -r '.program // "?"' "$valid" | sort | uniq -c | sort -rn | head -20
echo

echo "## comandos mais repetidos"
jq -r '.command // "?"' "$valid" | sort | uniq -c | sort -rn | head -20
echo

echo "## falhas por status"
jq -r 'select(.status != "ok") | .status' "$valid" | sort | uniq -c | sort -rn
echo

echo "## comandos que falharam"
jq -r 'select(.status != "ok") | [.status, (.exit_code // "-"), .program, .command] | @tsv' "$valid" | head -20
echo

echo "## mais lentos (ms)"
jq -r '[.duration_ms, .status, .program, .command] | @tsv' "$valid" | sort -rn | head -15
