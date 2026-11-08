# Quickstart: обернуть любого MCP-агента «жёсткой петлёй»

Шлюз — это обычный MCP-сервер по stdio (`pcagent --mcp`). Подходит любой
клиент, умеющий MCP: Claude Desktop, Cursor, Cline, собственный оркестратор.
Агент видит только девять инструментов и не знает, что за ними — Jev, Z3 и
задние ворота.

## 1. Подключение

Claude Desktop (`claude_desktop_config.json`) / Cursor (`.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "pcagent-jev": {
      "command": "C:\\Program Files\\PCAgent\\pcagent.exe",
      "args": ["--mcp"],
      "env": {
        "PCAGENT_GATE_PROFILE": "strict",
        "ENDOSCOPE_URL": "http://127.0.0.1:7098",
        "ENDOSCOPE_RUN_ID": "<id прогона>",
        "PCAGENT_COLLECTOR": "<команда сборщика, если нужны ролики>"
      }
    }
  }
}
```

На Linux/macOS — путь к бинарнику `pcagent`. Без эндоскопа шлюз маршруты
строит и шаги закрывает, но **печать не ставит** (`proof_failures:
«эндоскоп не настроен»`): для MCP-режима независимая проверка обязательна.

## 2. Инструменты шлюза

| Инструмент | Что делает | Кто решает |
|------------|-----------|------------|
| `jev_route {task, have}` | Маршрут из каталога операторов или `missing` с перечнем предпосылок; задача не из каталога → `unknown`, ничего не выдумывается | Jev |
| `jev_observe {route_id, step_id, probe}` | Шлюз сам наблюдает: `{"kind":"file","path"}`, `{"kind":"url","url"}`, `{"kind":"command","program","args"}`, `{"kind":"screen"}` → `witness_id`, SHA-256, байты. Только для текущего узла пути | код |
| `jev_step_done {route_id, step_id, witness_ids, note?}` | Закрыть шаг: свидетели принадлежат шагу, захваченное содержимое удовлетворяет `done_when`, порог профиля, зависимости по Z3. `note` — непроверенный комментарий | код + Z3 |
| `jev_collect {route_id}` | Сбор материалов коллекции шлюзом (исследование роликов) | код |
| `jev_close {route_id, report}` | Задние ворота: печать `jev_close: true` или перечень непройденного | код + Z3 + эндоскоп |
| `jev_status {id}` | Состояние пути, свидетели по шагам, журнал, `off_rail_attempts`, профиль | — |
| `jev_report {id}` | Итоговый ответ пользователю, собранный шлюзом | код |
| `jev_study`, `jev_close_study` | Корпус: изучить все фрагменты, закрыть по проверенным цитатам | код |

## 3. Полный цикл

```
1. jev_route {"task":"задеплой CRM на vercel","have":["env:pc","env:browser","env:git","env:vercel"]}
   → {"status":"route","route_id":"route-1","steps":[{"id":"deploy_check_repo","done_when":"в выводе виден чистый git status и ветка main", ...}, ...]}

   Если have неполный:
   → {"status":"missing","missing":["Есть доступ к Vercel, включая авторизованный токен"],"available_have":[...]}
   Это и есть строгий возврат: задача уходит пользователю с точным перечнем.

2. jev_observe {"route_id":"route-1","step_id":"deploy_check_repo",
                "probe":{"kind":"command","program":"git","args":["status","-b"]}}
   → {"status":"observed","witness":{"witness_id":"w-3f9a…","sha256":"…","bytes":212,"detail":"exit 0","text_head":"## main\nnothing to commit…"}}

3. jev_step_done {"route_id":"route-1","step_id":"deploy_check_repo","witness_ids":["w-3f9a…"]}
   → {"status":"done","remaining":[…],"jev_close":false,"next":"jev_step_done"}

   Попытка словами:
   jev_step_done {..., "evidence":"git status чистый"}  → refused: «параметр evidence упразднён…»
   Попытка вперёд по пути:
   jev_observe {step_id: последний шаг}                  → refused: «не на рельсах», журнал: off_rail

4. … так до последнего шага → {"all_steps_done":true,"jev_close":false,"next":"jev_close"}

5. jev_close {"route_id":"route-1","report":"…"}
   → {"status":"closed","seal":{"jev_close":true,"path":[…],"anchor":"<sha256 цепочки свидетелей>","proof":[…,"witness_anchor","endoscope"]},
      "assumptions":["env:pc",…],"off_rail_attempts":0}
   Или → {"status":"open","jev_close":false,"unvisited":[{"step_id":"…","why":"не начат"}], "reason":"…"}

6. jev_report {"id":"route-1"}
   → {"verified":true,"text":"ПОДТВЕРЖДЕНО ШЛЮЗОМ: route-1 закрыт, шагов пройдено 4, якорь …\nПуть: …\nСвидетелей шлюза: 4\nДопущения, заявленные вызывающей стороной (шлюзом не проверялись): env:pc, …"}
```

## 4. Правило для оркестратора / человека

Показывать пользователю «готово» можно **только** из `jev_report` с
`verified: true`. Всё, что агент пишет сам, — черновик. Если клиент даёт агенту
собственные инструменты (файлы, браузер), это не ломает петлю: действия мимо
шлюза не порождают свидетелей, а без свидетелей шаг не закрыть.

## 5. Частная сборка (custom)

```
PCAGENT_GATE_PROFILE=custom
PCAGENT_GATE_MIN_BYTES=256          # больше содержимого на шаг
PCAGENT_GATE_PROBES=command,url,screen   # запретить file: агент не покажет «свой» файл
```

Инварианты (свидетель обязателен, привязка к шагу, якорь в печати, эндоскоп)
профилем не отключаются.
