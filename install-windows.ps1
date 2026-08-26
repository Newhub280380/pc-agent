# Установка PC Agent на Windows одной командой (PowerShell):
#   irm https://pcagent-dl.vercel.app/win.ps1 | iex
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$base = 'https://pcagent-dl.vercel.app'
$dir  = Join-Path $env:LOCALAPPDATA 'PCAgent'
$exe  = Join-Path $dir 'pcagent.exe'
$cfg  = Join-Path $dir 'config.json'

New-Item -ItemType Directory -Force -Path $dir | Out-Null

Write-Host "`n== Качаю агента (20 МБ)"
$tmp = "$exe.tmp"
Invoke-WebRequest -Uri "$base/pcagent.exe" -OutFile $tmp

# Сверка с опубликованной суммой: без неё подмену файла на хостинге
# не видно, а запускается он с правами пользователя.
$want = (Invoke-WebRequest -Uri "$base/pcagent.exe.sha256" -UseBasicParsing).Content.Trim().Split(' ')[0]
$got  = (Get-FileHash -Algorithm SHA256 $tmp).Hash.ToLower()
if ($want -ne $got) {
  Remove-Item $tmp -Force
  throw "контрольная сумма не совпала — файл повреждён или подменён"
}
Move-Item -Force $tmp $exe

if (-not (Test-Path $cfg)) {
  Write-Host "`n== Ключ NVIDIA (nvapi-...), Enter чтобы пропустить"
  $key = Read-Host '   Ключ'
  if ([string]::IsNullOrWhiteSpace($key)) {
    '{ "llm_provider": "kilo" }' | Set-Content -Encoding UTF8 $cfg
  } else {
    @{
      llm_provider = 'nvidia'
      api_key      = $key
      base_url     = 'https://integrate.api.nvidia.com/v1'
      model        = 'meta/llama-3.2-90b-vision-instruct'
    } | ConvertTo-Json | Set-Content -Encoding UTF8 $cfg
  }
}

Write-Host "`n== Ярлык на рабочем столе"
$lnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'PCAgent.lnk'
$s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$s.TargetPath = $exe
$s.WorkingDirectory = $dir
$s.Save()

Write-Host "`n== Запускаю"
Start-Process $exe
Write-Host "Готово. Ярлык PCAgent на рабочем столе, файлы: $dir"
