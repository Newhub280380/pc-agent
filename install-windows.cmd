@echo off
chcp 65001 >nul
setlocal
set "DIR=%LOCALAPPDATA%\PCAgent"
set "EXE=%DIR%\pcagent.exe"
set "CFG=%DIR%\config.json"
set "URL=https://github.com/Newhub280380/pc-agent/releases/latest/download/pcagent.exe"

echo.
echo   Установка PCAgent
echo   -----------------
if not exist "%DIR%" mkdir "%DIR%"

rem pcagent.exe рядом со скриптом важнее скачивания: у приватного репозитория
rem релизы без авторизации не отдаются.
if exist "%~dp0pcagent.exe" (
  echo   [1/3] Беру pcagent.exe из этой же папки...
  copy /y "%~dp0pcagent.exe" "%EXE%" >nul
) else (
  echo   [1/3] Скачиваю агента (около 20 МБ) и сверяю контрольную сумму...
  powershell -NoProfile -ExecutionPolicy Bypass -Command ^
    "$ErrorActionPreference='Stop'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri '%URL%' -OutFile '%EXE%.tmp'; $w=(Invoke-WebRequest -Uri '%URL%.sha256' -UseBasicParsing).Content.Trim().Split(' ')[0]; $g=(Get-FileHash -Algorithm SHA256 '%EXE%.tmp').Hash.ToLower(); if ($w -ne $g) { Remove-Item '%EXE%.tmp' -Force; throw 'контрольная сумма не совпала' }; Move-Item -Force '%EXE%.tmp' '%EXE%'"
)
if not exist "%EXE%" goto fail

echo   [2/3] Нужен ключ NVIDIA (начинается на nvapi-).
set "KEY="
set /p KEY=        Вставьте ключ и нажмите Enter: 
if "%KEY%"=="" (
  echo        Ключ не введён — агент запустится, но думать не сможет.
  echo        Позже впишите его в %CFG%
  set "KEY=СЮДА_ВСТАВИТЬ_КЛЮЧ_nvapi"
)
> "%CFG%" echo {
>>"%CFG%" echo   "llm_provider": "nvidia",
>>"%CFG%" echo   "base_url": "https://integrate.api.nvidia.com/v1",
>>"%CFG%" echo   "api_key": "%KEY%",
>>"%CFG%" echo   "model": "meta/llama-3.2-90b-vision-instruct"
>>"%CFG%" echo }

echo   [3/3] Делаю ярлык на рабочем столе...
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Desktop')+'\PCAgent.lnk'); $s.TargetPath='%EXE%'; $s.WorkingDirectory='%DIR%'; $s.Save()"

echo.
echo   Готово. Ярлык PCAgent на рабочем столе, файлы здесь: %DIR%
start "" "%EXE%"
pause
exit /b 0

:fail
echo.
echo   Не получилось положить pcagent.exe в %DIR%.
echo   Скачайте pcagent.exe из релиза, положите рядом с этим файлом и запустите снова.
pause
exit /b 1
