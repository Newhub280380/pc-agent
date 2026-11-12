#!/usr/bin/env bash
# Хук Claude Code -> реестр авторства шлюза Jev (Linux/macOS).
# То же, что jev-claude-hook.ps1: режимы post-tool | session | scan.
# Окружение: JEV_URL, JEV_TOKEN, JEV_OUTPUT_DIR. Никогда не блокирует Claude.
mode="${1:-post-tool}"
log="${TMPDIR:-/tmp}/jev-hook.log"
marker="${TMPDIR:-/tmp}/jev-session.marker"

register() {
  local f="$1"
  [ -f "$f" ] || return 0
  local sha bytes
  if command -v sha256sum >/dev/null; then sha=$(sha256sum "$f" | cut -d' ' -f1)
  else sha=$(shasum -a 256 "$f" | cut -d' ' -f1); fi
  bytes=$(wc -c < "$f" | tr -d ' ')
  local body
  body=$(jq -cn --arg s "$sha" --argjson b "$bytes" --arg l "$f" \
    '{jsonrpc:"2.0",id:1,method:"tools/call",params:{name:"jev_register_artifact",arguments:{sha256:$s,bytes:$b,locator:$l}}}')
  echo "$(date -Is) $sha $f -> $(curl -sS -m 10 -H "Authorization: Bearer $JEV_TOKEN" \
    -H 'Content-Type: application/json' -d "$body" "$JEV_URL")" >> "$log" 2>&1
}

{
  [ -n "$JEV_URL" ] && [ -n "$JEV_TOKEN" ] || { echo "$(date -Is) JEV_URL/JEV_TOKEN не заданы"; exit 0; }
  case "$mode" in
    session) touch "$marker" ;;
    post-tool)
      f=$(jq -r '.tool_input.file_path // .tool_input.notebook_path // empty')
      [ -n "$f" ] && register "$f" ;;
    scan)
      [ -n "$JEV_OUTPUT_DIR" ] || exit 0
      if [ -f "$marker" ]; then newer=(-newer "$marker"); else newer=(-mmin -720); fi
      find "$JEV_OUTPUT_DIR" -type f "${newer[@]}" -print0 |
        while IFS= read -r -d '' f; do register "$f"; done ;;
  esac
} >> "$log" 2>&1
exit 0
