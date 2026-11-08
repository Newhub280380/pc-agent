# Задние ворота (back-gate closure): флаг `jev_close`

Документ описывает, как в «жёсткой петле» устроено закрытие задачи через
задние ворота: что такое печать закрытия, кто и когда её ставит, как это
подключено к MCP-шлюзу и к встроенному агенту, и как внешнему
оркестратору показывать пользователю «готово» только при наличии флага.

## Зачем

Петля устроена так: Jev прокладывает путь (граф шагов или список
фрагментов корпуса), агент идёт по нему как технический исполнитель и в
конце концов оказывается на максимальной глубине — последний шаг пути
закрыт, последний фрагмент изучен. В этой точке нужен **возврат**: задача
либо закрывается с подтверждением, либо возвращается пользователю с точным
перечнем того, что не пройдено.

До этого изменения точка возврата была размыта:

* в MCP-шлюзе `jev_step_done` для последнего шага возвращал `remaining: []`
  и ничего не говорил о закрытии, а `jev_close` проверял Z3 и эндоскоп, но
  итоговое «закрыто» было просто удалением маршрута из памяти — без
  печати, без реестра закрытых, и с внутренним методом
  `close_after_proof`, который закрывал маршрут в обход проверки пути;
* во встроенном агенте `TaskReport::closed(text)` можно было вызвать из
  любого места — флаг `closed: true` был обычным `bool`;
* ни в одном месте не проверялся **порядок** прохождения
  (`completed_order`): Z3 доказывал только «все узлы Done и подтверждены»,
  но не «зависимости закрыты раньше зависимых».

## Что сделано

### Модуль `core/src/back_gate.rs`

Единственное место, где рождается флаг закрытия.

* **`Seal`** — печать закрытия. У структуры приватное поле `_private: ()`,
  поэтому сконструировать её вне модуля невозможно: единственный способ
  получить `Seal` — пройти `seal_route` или `seal_study`. Печать несёт
  предмет (`route-N` / `study-N`), фактический путь, список проверок
  (`proof`) и время закрытия. `Seal::json()` — единственный код, который
  пишет `"jev_close": true`.
* **`Refusal`** — отказ в закрытии: `unvisited` (непройденные шаги и их
  состояние), `unconfirmed` (пройденные без доказательства или без
  подтверждения шлюза), `out_of_order` (нарушения порядка),
  `proof_failures` (Z3, эндоскоп), `extra` (предметные причины — отклонённые
  утверждения отчёта и т.п.). `Refusal::json()` всегда даёт
  `"status": "open", "jev_close": false` и строку `reason`.
* **`seal_route(subject, graph, observed, verdict, policy)`** — проверка
  маршрута, строго по порядку:
  1. каждый узел графа в состоянии `Done` (`Pending`/`Running`/`Failed`/
     `RolledBack`/`Skipped` → `unvisited`);
  2. у каждого узла есть `note` (доказательство) и `observed[id] == true`
     (подтверждение шлюза, а не слова агента) → иначе `unconfirmed`;
  3. журнал `completed_order` содержит каждый узел ровно один раз, лишних
     нет, все зависимости стоят строго раньше → иначе `out_of_order`;
  4. Z3: `logic::check_solution` (как раньше) и **новый**
     `logic::prove_walk_order` — позиции узлов фиксируются их местом в
     журнале, для каждой зависимости требуется `pos_i > pos_j`; пропуск,
     повтор и чужой узел — именованные ложные утверждения, которые
     всплывают в unsat-ядре (`walk_unvisited_n3`,
     `walk_order_n2_after_n1`, …);
  5. приговор эндоскопа закрывает задачу (`Proven`; для `NotConfigured`
     решает `EndoscopePolicy`).
* **`seal_study(subject, sources, walk, report, verdict, policy)`** — то же
  для исследования корпуса поверх `corpus::back_gate`: источники полны,
  все фрагменты изучены шлюзом, каждое утверждение отчёта проверено,
  эндоскоп согласен. Возвращает печать и проверенный текст отчёта.
* **`EndoscopePolicy`**: `Required` — MCP-шлюз (без эндоскопа закрытия
  нет, как и раньше), `Optional` — встроенный агент (без эндоскопа
  закрывают проверки ядра, поведение GUI/headless не изменилось).

### Z3: `logic::prove_walk_order`

Добавлен в `core/src/logic.rs` рядом с `prove_schedule`/`check_solution`.
Отличие от `prove_schedule`: там солвер *ищет* расписание, здесь он
*проверяет* фактическое — позиции не свободные переменные, а константы из
журнала прохождения. Поэтому подделанный журнал (зависимый шаг закрыт
раньше предпосылки) — UNSAT, и в отказе названа пара узлов.

### MCP-шлюз `core/src/jev_gate.rs`

* `Gate.closed: HashMap<String, Seal>` — реестр закрытых маршрутов и
  исследований. Запись делают только `close`/`close_study` после печати.
* `jev_step_done` для последнего шага (точка возврата с глубины)
  возвращает `all_steps_done: true, jev_close: false, next: "jev_close"`:
  путь пройден, но задача ещё **не** закрыта.
* `jev_close`:
  * если `route_id` уже в реестре — `refused` «уже закрыт», с печатью;
  * для исследовательского маршрута — как раньше, сначала проверка
    отчёта по корпусу (`ledger::check_report`), затем сводка;
  * эндоскоп вызывается **только** если путь структурно цел (иначе
    независимая проверка тратится впустую, а отказ и так известен);
  * печать ставит `back_gate::seal_route`; ответ —
    `{"status":"closed","route_id":…,"seal":{"jev_close":true,…}}`;
  * любой провал — `{"status":"open","jev_close":false,"unvisited":[…],
    "unconfirmed":[…],"out_of_order":[…],"proof_failures":[…],"reason":…}`.
* `jev_close_study` — аналогично через `back_gate::seal_study`.
* Удалены обходные пути: метод `close_after_proof` (закрывал маршрут без
  проверки пути) и `close_endoscope_verdict`. Подмена приговора эндоскопа
  осталась только под `#[cfg(test)]` (`Gate.judge`), в боевой сборке поля
  нет.
* Новый инструмент **`jev_status {id}`** — состояние маршрута или
  исследования глазами задних ворот: `closed` + печать, либо `open` +
  перечень непройденного, состояния шагов и `completed_order`. Печать не
  ставит, эндоскоп не вызывает.

### Встроенный агент `core/src/agent/mod.rs`

* `TaskReport::closed(&Seal, text)` — закрытый отчёт теперь требует
  печать; без значения `Seal` собрать `closed: true` нельзя.
* `close_if_proven` вызывает `back_gate::seal_route` дважды: сначала
  структурно (до эндоскопа, чтобы вернуть точный перечень), затем с
  приговором эндоскопа. Отказ — `TaskReport::open("Задача не закрыта: не
  пройдены шаги: n3 (не начат), n4 (не начат); …")`.
* Ужесточение: откаченный (`RolledBack`) узел без зависимых раньше не
  мешал `check_solution`, теперь он — непройденный шаг пути.

## Как подключить к MCP-шлюзу

Запуск сервера не изменился:

```json
{"command":"pcagent","args":["--mcp"]}
```

Правило для клиента/оркестратора: **подтверждение пользователю
показывается только при `jev_close: true` в ответе `jev_close` /
`jev_close_study` / `jev_status`.** Всё остальное — задача открыта, и
пользователю отдаётся `reason` (или структурированные списки).

### Пример: полный проход

```jsonc
// 1. маршрут
{"name":"jev_route","arguments":{"task":"отрендери ролик с заменой лица","have":["env:pc","env:video_file"]}}
// → {"status":"route","route_id":"route-1","steps":[{"id":"n1",...},{"id":"n2",...},{"id":"n3",...}]}

// 2. шаги по порядку, каждый с доказательством (фрагменты done_when)
{"name":"jev_step_done","arguments":{"route_id":"route-1","step_id":"n1","evidence":"…"}}
// → {"status":"done","step_id":"n1","remaining":["n2","n3"],"all_steps_done":false,"jev_close":false,"next":"jev_step_done"}
// …
{"name":"jev_step_done","arguments":{"route_id":"route-1","step_id":"n3","evidence":"…"}}
// → {"status":"done","step_id":"n3","remaining":[],"all_steps_done":true,"jev_close":false,"next":"jev_close"}

// 3. задние ворота
{"name":"jev_close","arguments":{"route_id":"route-1","report":"…"}}
// → {"status":"closed","route_id":"route-1",
//    "seal":{"jev_close":true,"subject":"route-1","steps":3,"path":["n1","n2","n3"],
//            "proof":["steps_done","evidence","observed","order","z3_solution","z3_order","endoscope"],
//            "closed_at":"2026-11-09T10:15:00+00:00"}}
```

### Пример: частичное прохождение

```jsonc
{"name":"jev_close","arguments":{"route_id":"route-1","report":"готово"}}
// → isError: true
// {"status":"open","jev_close":false,"subject":"route-1",
//  "unvisited":[{"step_id":"n3","why":"не начат"}],
//  "reason":"не пройдены шаги: n3 (не начат)"}
```

### Пример: повторное закрытие

```jsonc
{"name":"jev_close","arguments":{"route_id":"route-1","report":"готово ещё раз"}}
// → isError: true
// {"status":"refused","reason":"маршрут route-1 уже закрыт шлюзом; повторное закрытие невозможно","seal":{...}}
```

### Пример: статус

```jsonc
{"name":"jev_status","arguments":{"id":"route-1"}}
// открытый: {"status":"open","jev_close":false,"id":"route-1","unvisited":[…],
//            "steps":[{"id":"n1","state":"done","observed":true},…],"completed_order":["n1"]}
// закрытый: {"status":"closed","id":"route-1","seal":{"jev_close":true,…}}
```

## Что печать не делает

* Не проверяет текст отчёта сама — для исследований это делает
  `ledger::check_report` / `corpus::back_gate`, для остального — эндоскоп.
* Не переживает перезапуск процесса: реестр `closed` живёт в памяти
  MCP-сервера, как и маршруты.
* Не заменяет эндоскоп: `Required` в шлюзе означает, что без настроенного
  эндоскопа (`ENDOSCOPE_URL`/`ENDOSCOPE_RUN_ID`) маршрут остаётся открытым
  с причиной «эндоскоп не настроен».

## Тесты

* `back_gate::tests` — полный проход, частичный, без подтверждения,
  подделанный порядок, откат, политика эндоскопа, пустой граф.
* `logic::tests::walk_order_proof_*` — Z3-инвариант порядка.
* `jev_gate::tests` — `full_walk_with_proven_endoscope_gets_the_seal`,
  `partial_walk_is_refused_with_exact_unvisited_list`,
  `premature_close_lists_every_step_and_keeps_route_open`,
  `second_close_is_refused_and_seal_is_kept`,
  `disproven_endoscope_blocks_the_seal`,
  `endoscope_is_not_consulted_for_an_unfinished_route`,
  `research_route_is_sealed_only_after_every_reel_and_claim_check`,
  `status_of_unknown_id_is_refused`.

Запуск: `cd core && cargo fmt --check && cargo test`.
