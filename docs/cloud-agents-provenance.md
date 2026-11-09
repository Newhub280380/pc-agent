# Облачные агенты (Grok и др.) на петле Jev: кто что сделал на самом деле

## Проблема
Облачный агент принёс дизайн и сказал «это сделал Claude», хотя сделал сам.
Слова агента об авторстве — выдумка того же сорта, что «на экране видно…».
Облачного агента нельзя «заглянуть внутрь», поэтому проверять надо не его,
а **артефакт**: по хэшу содержимого и по реестру, куда автор пишет только сам
за себя.

## Как устроено (без LLM в точке решения)
1. Шлюз `pcagent --mcp-http` — тот же Jev-шлюз, но по HTTP (MCP Streamable
   HTTP), чтобы облачные боты могли подключиться.
2. У каждого агента свой токен. **Личность берётся из токена**, а не из слов
   агента: назваться Claude агент grok-1 не может.
3. `jev_register_artifact` — агент регистрирует **свою** работу. Автором
   пишется владелец токена. Хэш считает шлюз (file/url) или хук Claude Code.
   Кто зарегистрировал хэш первым — тот и автор; повторная регистрация чужого
   хэша отклоняется («автор он, а не ты»).
4. `jev_attest` — перед словами «это сделал X» агент обязан спросить шлюз.
   Шлюз сам скачивает/читает файл, считает SHA-256 и сверяет с реестром:
   * `confirmed` — правда;
   * `mismatch` — **выдумка**, засчитывается агенту в сводке;
   * `unproven` — никто этот файл не регистрировал, авторство не доказано.
5. `jev_agents` — сводка: вызовы, отказы, пойманные выдумки по каждому агенту.
6. Маршруты (`jev_route` → `jev_observe` → `jev_step_done` → `jev_close`)
   принадлежат построившему их агенту — пять ботов не закрывают шаги друг за
   друга.
7. Облачному агенту запрещено наблюдать хост: только `url` и файлы внутри
   `--file-root`. `command` и `screen` — только локально (stdio).

## Запуск шлюза
```
pcagent --mcp-http 0.0.0.0:3000 --agents agents.txt --file-root ./artifacts
```
`agents.txt` — строки `имя токен` (токен ≥ 16 символов, например
`openssl rand -hex 24`). Реестр на диске: `PCAGENT_PROVENANCE_FILE=provenance.jsonl`.
Шлюз должен быть доступен из интернета по HTTPS (облако, Cloudflare Tunnel,
ngrok) — облачный Grok не видит `localhost`.

## Подключение Grok
* **xAI API** (боты через API): в `tools` добавить
  `{"type":"mcp","server_url":"https://<хост>/mcp","server_label":"jev","authorization":"<токен агента>"}`.
* **Grok Bot / grok.com Connectors** (где нельзя задать заголовок):
  кастомный MCP-сервер с адресом `https://<хост>/mcp/<токен агента>`.
  Каждому боту — свой токен.

Инструкция боту (system prompt):
> Перед тем как сказать, кто сделал файл/дизайн, вызови `jev.jev_attest`
> с этим файлом и автором. Сообщай пользователю только `verdict` шлюза.
> Свою работу регистрируй `jev.jev_register_artifact` сразу после создания.

## Хук Claude Code (регистрирует всё, что пишет Claude)
`.claude/settings.json` (Windows):
```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:/pc-agent/scripts/jev-claude-hook.ps1 session"}]}],
    "PostToolUse": [{"matcher": "Write|Edit|MultiEdit|NotebookEdit", "hooks": [{"type": "command", "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:/pc-agent/scripts/jev-claude-hook.ps1 post-tool"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:/pc-agent/scripts/jev-claude-hook.ps1 scan"}]}]
  }
}
```
Переменные окружения: `JEV_URL=https://<хост>/mcp`, `JEV_TOKEN=<токен claude>`,
`JEV_OUTPUT_DIR=<папка, куда Claude кладёт картинки>`. Для Linux/macOS —
`scripts/jev-claude-hook.sh` с теми же режимами. Хук не блокирует Claude;
журнал — `%TEMP%\jev-hook.log`.

## Честные пределы
* Изменённый/пережатый файл — другой хэш → `unproven`. Это строгость.
* Агент может вообще не вызвать `jev_attest` и соврать в чате. Правило для
  пользователя: **отчёт без verdict шлюза = не подтверждён**. Проверить сам
  можно, вызвав `jev_attest` от токена `admin`.
* Режим `scan` регистрирует на Claude все новые файлы в `JEV_OUTPUT_DIR`.
  Не кладите туда чужие картинки во время сессии Claude.
