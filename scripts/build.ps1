# Сборка PC Agent в один .exe под Windows x64.
#
# Порядок важен: Go-роутер собирается ПЕРВЫМ, потому что build.rs ядра
# вшивает его бинарь через include_bytes! — иначе в .exe попадёт пустышка.
#
# Требования (ставятся один раз):
#   - Rust (rustup, MSVC toolchain)
#   - Go 1.21+
#   - Visual Studio Build Tools 2022 + Windows 10/11 SDK (C++/WinRT, UIA, WIC)
#
# Запуск:  powershell -ExecutionPolicy Bypass -File scripts\build.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"

Write-Host "== 1/3 Go LLM Router ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "router")
$env:CGO_ENABLED = "0"
$env:GOOS = "windows"
$env:GOARCH = "amd64"
go vet ./...
# -s -w срезают отладочную информацию: бинарь ~6 МБ вместо ~9 МБ, а он
# целиком уезжает внутрь .exe ядра.
go build -ldflags "-s -w -X main.Version=1.0.0" -o pcagent-router.exe .
Pop-Location

Write-Host "== 2/3 Rust core + C++ native ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "core")
cargo build --release
Pop-Location

Write-Host "== 3/3 dist ==" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item (Join-Path $root "core\target\release\pcagent.exe") $dist -Force
Copy-Item (Join-Path $root ".env.example") $dist -Force

$exe = Join-Path $dist "pcagent.exe"
$size = [math]::Round((Get-Item $exe).Length / 1MB, 1)
Write-Host "Готово: $exe ($size МБ)" -ForegroundColor Green
Write-Host "Двойной клик по нему создаст .env, ярлык на рабочем столе и запросит согласие."
