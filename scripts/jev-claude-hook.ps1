# Хук Claude Code -> реестр авторства шлюза Jev.
# Каждый файл, который Claude записал (Write/Edit), регистрируется на Claude
# по SHA-256. Тогда облачный агент не сможет выдать свой дизайн за работу
# Claude: jev_attest вернёт mismatch.
#
# Режимы:
#   post-tool    — PostToolUse: путь файла берётся из JSON на stdin.
#   session      — SessionStart: ставит отметку начала сессии.
#   scan         — Stop: регистрирует файлы из $env:JEV_OUTPUT_DIR, изменённые
#                  после отметки (картинки, которые Claude делал через скрипты).
#
# Окружение: JEV_URL (https://.../mcp), JEV_TOKEN (токен агента claude),
# JEV_OUTPUT_DIR (необязательно). Хук никогда не блокирует Claude: любые
# ошибки пишутся в %TEMP%\jev-hook.log, выход всегда 0.
param([string]$Mode = "post-tool")
$ErrorActionPreference = "Stop"
$log = Join-Path $env:TEMP "jev-hook.log"
$marker = Join-Path $env:TEMP "jev-session.marker"

function Register-File([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    $item = Get-Item -LiteralPath $Path
    $sha = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLower()
    $body = @{
        jsonrpc = "2.0"; id = 1; method = "tools/call"
        params  = @{ name = "jev_register_artifact"; arguments = @{
            sha256 = $sha; bytes = [int64]$item.Length; locator = $item.FullName } }
    } | ConvertTo-Json -Depth 6 -Compress
    $resp = Invoke-RestMethod -Uri $env:JEV_URL -Method Post -TimeoutSec 10 `
        -Headers @{ Authorization = "Bearer $($env:JEV_TOKEN)" } `
        -ContentType "application/json; charset=utf-8" `
        -Body ([Text.Encoding]::UTF8.GetBytes($body))
    Add-Content $log "$(Get-Date -Format o) $sha $($item.FullName) -> $($resp.result.content[0].text)"
}

try {
    if (-not $env:JEV_URL -or -not $env:JEV_TOKEN) { throw "JEV_URL/JEV_TOKEN не заданы" }
    switch ($Mode) {
        "session" { Set-Content $marker (Get-Date -Format o) }
        "post-tool" {
            $in = [Console]::In.ReadToEnd() | ConvertFrom-Json
            $p = $in.tool_input.file_path
            if (-not $p) { $p = $in.tool_input.notebook_path }
            if ($p) { Register-File $p }
        }
        "scan" {
            if (-not $env:JEV_OUTPUT_DIR) { break }
            $since = if (Test-Path $marker) { [datetime](Get-Content $marker) } else { (Get-Date).AddHours(-12) }
            Get-ChildItem -LiteralPath $env:JEV_OUTPUT_DIR -File -Recurse |
                Where-Object { $_.LastWriteTime -ge $since } |
                ForEach-Object { Register-File $_.FullName }
        }
    }
} catch {
    Add-Content $log "$(Get-Date -Format o) ERROR $Mode $($_.Exception.Message)"
}
exit 0
