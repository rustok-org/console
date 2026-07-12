# Console v2 Фаза 2, Этап 2 — резидентность + nav-shell + двух-блочный From→To

> Статус: **ПРЕДЛОЖЕНИЕ, ждёт Гейта-1.** Кода нет до «go».
> Управляющие доки: `meta/docs/CONSOLE-V2-SPEC.md` (§2 виды, §5 Фаза 2, §9.5 порядок),
> протокол `docs/APPROVER-PROTOCOL.md` (proto 2 — канон на main после core#91/console#9).
> Ратификация порядка этапов: workflow-state 2026-07-12T21:55 (Receive = Этап 3, после этого).
> Команда: Инженер/Ревьюер/Капитан; связь Инженер↔Ревьюер только через Капитана.

## Цель — один абзац

Консоль перестаёт выходить по решению: после `approve`/`deny` человек возвращается к очереди
и продолжает работать — это ядро запроса Капитана «оставаться в приложении». Плюс два
довеска, зависящих от той же правки: nav-shell (каркас переключения видов, в который Этап 3
воткнёт Receive, а Этап 5 — Дашборд) и двух-блочный From→To на карточке (адрес отправителя
теперь доступен через `context`-op, proto 2). Плюс закрытие двух дыр, найденных /check:
экран PIN-lockout в резидентном режиме и клиентская политика read-op'ов.

## Замер blast radius (сделан ДО спеки, по требованию /check-финдинга Этапа 1)

Правка «не выходим после решения» ломает/меняет **~13 существующих тестов из 137**
(не ~30, как у авто-открытия — та цифра была про другую правку):

- `app.rs` (7): `an_executed_answer_exits_approved_and_keeps_the_tx_hash` (:1773),
  `a_failed_broadcast_exits_failed_not_approved` (:1787), `a_human_deny_exits_rejected`
  (:1800), `the_deadline_denies_fail_closed_and_exits_expired_not_rejected` (:1811),
  `an_item_executed_by_another_connection_exits_approved_even_if_we_denied` (:1831),
  `an_already_expired_item_exits_expired` (:1847), `terminal_exit_maps_every_answer` (:1865).
- `main.rs` (4): `exit_code_distinguishes_every_decision` (:489),
  `the_decision_line_names_what_happened_to_the_money` (:650),
  `the_decision_line_survives_a_hostile_server_reason` (:697),
  `the_decision_speaks_only_after_the_terminal_is_restored`.
- `ui.rs`: рендер-диспетч `Phase::Resolved` (:39, `resolved_text` :45) — экран уходит.
- `protocol.rs`: hello-фикстуры `proto:1` (:531/:537) — клиент переходит на proto 2.

Производственный код: `apply_resolve` (терминальная ветка :741), `on_reply` (:653 —
«terminal answer ends the session»), `terminal_exit`/`ExitOutcome`, run-loop `main.rs`
(:150-165 Resolved-arm), `decision_line`/`exit_code`.

## ⚠️ Коллизия с инвариантом AGENTS.md #7 — вопрос ратификации №1

Инвариант #7: «UI → stderr, machine decision → stdout; exit codes: approved / rejected /
expired / aborted / no-tty» — контракт **one-shot** эпохи: одно решение = один процесс =
один exit-код. В резидентной консоли решений за сессию много — exit-код физически не может
нести «решение». ТЗ одновременно требует резидентность (§5 Фаза 2) и «exit-коды #7 — не
ослаблять» (§4.4) — эти два требования в буквальном чтении несовместимы, нужна ратификация.

**Рекомендация (а):** эволюция контракта, не слом духа —
- **decision line на stdout остаётся, но ПО РЕШЕНИЮ — и только когда stdout не TTY**
  (finding /check-2 этого круга): в `docker exec -it` stdout = тот же TTY, что и
  alternate-screen на stderr — запись посреди сессии легла бы поверх кадра (ровно от
  этого сегодняшний `restore_then_announce`, main.rs:82). Пайп/скрипт (stdout не TTY) —
  newline-delimited stream тех же JSON-строк (`{"decision":"approved","tx_hash":"0x…"}`)
  в момент каждого решения; интерактивный человек — outcome-баннер в UI (stdout молчит).
  `IsTty`-проверка уже есть (инвариант #4, main.rs).
- **exit-коды сессии остаются**: aborted (quit), no-tty, fatal/upgrade — как были.
  **Per-decision коды (approved/rejected/expired) уходят** — их смысл переехал в stream.
- AGENTS.md #7 правится в этом PR: «machine decisions → stdout (one JSON line per
  decision, emitted only when stdout is not a TTY); exit codes report the session end:
  aborted / no-tty / fatal / upgrade» — все четыре сохраняемых кода явно (МИНОР Ревьюера,
  Гейт-1: `EXIT_UPGRADE=2` — отдельный протестированный код, не подслучай fatal).
  Решение оформляется отдельным ADR `.claude/decisions/2026-07-12-invariant-7-decision-stream.md`
  (предложение Ревьюера — обоснование инварианта не хоронить в архивной спеке).

**(б) почему:** дух инварианта — «машине есть что парсить, UI не мешает» — сохранён
полностью; буква «exit-код = решение» умирает вместе с one-shot моделью, которую Капитан
ратифицировал заменить. **(в) контраргумент:** скриптовые вызыватели, читающие exit-код
(mcp e2e-приёмка!), сломаются — но см. ниже: они пинят версию образа и обновятся на
релизном поезде §8, не сейчас. **(г) цена ошибки:** если контракт нужен кому-то ещё,
узнаем на ревью mcp-бампа, откат дёшев (режим one-shot можно вернуть флагом, но НЕ строим
его заранее — правило №5). **(д) что изменило бы выбор:** существование внешнего
потребителя exit-кодов, не контролируемого нами (нет такого: консоль ставится только
нашим образом).

**Downstream-эффект (заметить, не чинить здесь):** `mcp/tests/e2e` приёмка полагается на
one-shot поведение — она живёт против пина `CONSOLE_IMAGE` и обновится при бампе образа
(релизный поезд §8), не в этом PR.

## Скоуп PR — что входит / что ЯВНО не входит

**Входит (репо `console` целиком, core не трогаем — `context` уже на main):**

1. **Резидентность.** `apply_resolve`: терминальный ответ больше НЕ ставит
   `Phase::Resolved` — карточка закрывается, человек возвращается в `Watching` с
   **outcome-баннером** (транзиентная строка: `executed 0x…` тилом / `denied` красным /
   `expired` / `failed: reason` — те же тексты, что сегодня в `resolved_text`), list-поллинг
   возобновляется. `Phase::Resolved` и `ExitOutcome` удаляются; `terminal_exit`
   переименовывается в классификатор терминальности (Option<DecisionKind> для баннера и
   decision line). `Fatal` остаётся фатальным (транспорт мёртв — выходим как сейчас).
   Deadline-экспайри открытой карточки — как сейчас (авто-deny), но после ответа — баннер
   `expired` и возврат в очередь.
2. **Decision stream** (при ратификации №1): JSON-строка решения на stdout в момент
   каждого терминального ответа; правка AGENTS.md #7.
3. **PIN-lockout в резидентном режиме** (finding /check Этапа 1): ответ
   `Locked{retry_after_s}` или `BadPin{attempts_left:0}` (arming response, протокол §3.2/§4)
   на approve-пути → карточка **закрывается** (сервер fail-closed уже уронил pending в
   denied — держать карточку открытой «ещё approvable» = врать), в `Watching` ставится
   **locked-баннер с обратным отсчётом** (`retry_after_s`, тикает через `now_unix`, уже
   прокинутый в render) + текст «PIN locked — pending items were denied» (именно
   *pending*: уже Executing item fail-closed НЕ трогает, протокол §4 — текст не должен
   хоронить живую подпись). Пока lockout активен, approve/deny заблокированы (submit не
   диспетчится); list-поллинг продолжается (покажет реальность). **Отсчёт advisory:**
   ladder серверный — если после истечения клиентского отсчёта approve снова отвечает
   `locked` (skew), баннер ре-армится с новым `retry_after_s`, не фаталит. Auth-путь лока
   уже обрабатывается (`AuthError::Locked`) — не трогаем.
   **Приоритет транзиентных нот** (одно место в UI): locked-баннер > outcome-баннер >
   нота «item vanished».
4. **Client proto 2 + `context`.** `protocol.rs`: `PROTO_VERSION` 1→2,
   `Request::Context`, парс ответа (`address` + `balances` + `allowed_chains`, поле
   `wallet_locked`/`internal` ошибки). После `AuthOutcome::Ok` консоль шлёт `context`
   **один раз** (не поллит), адрес сохраняется в Model на сессию. Балансы в Этапе 2 не
   рендерятся (Дашборд = Этап 5) — но парсятся (тип полный, чтобы Этап 5 не трогал протокол).
5. **Двух-блочный From→To на карточке**: блок «From: your wallet / <адрес EIP-55, полный,
   verbatim>» → направление → «To: <адрес полный, verbatim>» (+ существующие поля).
   Строки — **внутри `priority_lines()`** (гейт `priority_fields_fit` их считает — как в
   Фазе 1). Статично, без мотиона (§6 ТЗ, ратифицировано). **Деградация:** `context`
   недоступен (`wallet_locked`/`internal`/старый сервер) → карточка Фазы 1 (только To),
   транзиентная нота; approve НЕ блокируется — From-блок дисплейный, подписной критикал
   (To/amount/decoded) не зависит от него.
6. **Nav-shell (минимум под Этап 3+):** `View`-состояние в Model (в Этапе 2 —
   единственный вид Queue/Approve), таб-бар в рендере (показывает зарегистрированные виды
   и их клавиши), маршрутизация клавиш через вид. НЕ строим заглушки Dashboard/Receive/
   History (правило №5) — только каркас, в который Этап 3 добавит второй вид.
7. **Клиентская политика read-ops** (направление Ревьюера): `context` уходит в сокет только
   когда нет открытой карточки и ничего in-flight — тем же `in_flight`-примитивом, каким
   `on_tick` гейтит `list` (:453). В Этапе 2 это тривиально выполняется (один запрос сразу
   после auth, до первой карточки); политика фиксируется в спеке как контракт для Этапов 3/5.

**ЯВНО НЕ входит:**
- Receive/QR (Этап 3), Дашборд/балансы/позиции (Этап 5), Activity (Этап 7), core-правки
  (PR2/PR3), swap (Фаза 2.5), демон (Фаза 3).
- **Реконнект после обрыва сокета** (finding /check): резидентная консоль с упавшим
  транспортом выходит через `Fatal`, как сегодня. Долгоживущее переподключение — территория
  демона (Фаза 3), здесь осознанно НЕ строим.
- Fallback клиента на proto 1 против старого сервера — НЕ строим (вопрос ратификации №2
  ниже).
- Авто-открытие единственного pending (issue #7 п.2) — свой круг, как и было.
- Мотион From→To (§6 ТЗ — статично, ратифицировано).
- Правка mcp e2e-приёмки (релизный поезд §8).

## Затронутые файлы

- `src/app.rs` — apply_resolve/on_reply/Model (address, locked_until, view), удаление
  Phase::Resolved/ExitOutcome, контекст-запрос после auth.
- `src/main.rs` — run-loop (уходит Resolved-arm), decision line per-decision, exit-коды
  сессии, map_key через вид.
- `src/ui.rs` — outcome-баннер, locked-баннер с отсчётом, From→To в priority_lines,
  таб-бар; уходит resolved-экран.
- `src/protocol.rs` — PROTO_VERSION=2, Request::Context, ContextOutcome + парс.
- `src/transport.rs` — прокладка нового Request/Reply (механически).
- `AGENTS.md` — инвариант #7 (при ратификации №1).
- `docs/APPROVER-PROTOCOL.md` — НЕ трогаем (канон уже описывает proto 2 целиком).

## Критерии приёмки — «PR готов, когда…»

1. **Резидентность red-мутацией** (ТЗ §5): тест, который падает, если apply_resolve снова
   ставит терминальную фазу — после `executed`/`denied`/`expired`/`failed` модель в
   `Watching`, карточки нет, баннер стоит, следующий tick шлёт `list`.
2. Каждое терминальное решение печатает JSON-строку на stdout, **только когда stdout не
   TTY** (тест на writer-проб + tty-ветку), формат идентичен сегодняшнему `decision_line`;
   в интерактиве stdout молчит (кадр TUI не рвётся).
3. Lockout: `Locked`/`BadPin(0)` на approve → карточка закрыта, баннер с отсчётом
   (снапшот на границе секунд), approve/deny заглушены до истечения, list-поллинг жив.
4. From→To: снапшоты «два блока помещаются» и «TOO SMALL → approve заблокирован»
   (`priority_fields_fit` держится); адреса в обоих блоках полные EIP-55 verbatim
   (регрессия address-poisoning — существующий verbatim-тест расширен на From-блок).
5. Деградация без `context`: карточка Фазы 1 + нота; approve работает (тест).
6. `hello` шлёт `proto:2`; против ответа `unsupported_proto supported:[1]` — честный
   upgrade-hint и выход (сегодняшнее поведение, тест обновлён).
7. Инварианты целы: PIN на high-risk, default-deny по дедлайну, «no key material»,
   verbatim-рендер, `priority_fields_fit`; никаких новых зависимостей (`cargo deny`).
8. Гейты зелёные с приложенным выводом: `cargo fmt --check`, `cargo clippy --all-targets
   -- -D warnings`, `cargo test`, `cargo deny check`. Новые тесты — red→green.

## Разделение ревью (распоряжение Капитана 2026-07-12)

Само-ревью Инженера = дешёвые гейты (`fmt`/`clippy`/`test`/`deny`) + прочтение дифа глазами.
**Флот-5 и `/security-review`/`/rust-review` НЕ гоняю — их проводит Ревьюер на Гейте-2.**
(В Этапе 1 флот запускал Инженер — исправлено, больше не повторяется.)

## Definition of Done

merged · ветка удалена · CI зелёный · AGENTS.md #7 синхронизирован · workflow-state
отметка «Этап 2 done» · ручной smoke в pty против стаба proto 2 заскринен.

## Тест-план

- **Поведение:** red-мутация резидентности (п.1 приёмки); lockout-флоу (bad_pin×3 на
  high-risk → arming → баннер → отсчёт → разблокировка по истечении); deadline-экспайри →
  баннер expired → очередь.
- **Протокол:** hello proto:2 (+ upgrade-hint на unsupported), context парс (ok /
  wallet_locked / internal / мусор → protocol error), full-типы балансов.
- **Снапшоты `TestBackend`:** From→To (обычный send · high-risk · граница TOO SMALL ·
  деградация без context) · outcome-баннер (executed/denied/expired/failed) ·
  locked-баннер · таб-бар. Каждый снапшот — диф прочитан.
- **Stdout-контракт:** decision line per-decision, UI не на stdout (writer-проба, как
  существующий `the_decision_speaks_only_after_the_terminal_is_restored` — переработан).
- **Пересмотр 13 замеренных тестов:** каждый либо переписан под новое поведение, либо
  осознанно удалён с заменой (в отчёте — таблица «был → стал»).

## Вопросы Гейта-1 (Ревьюеру и Капитану через Капитана)

1. **Инвариант #7 → decision stream** (см. блок выше) — ратифицировать вариант (а)?
   Это изменение инварианта репо, «do not break in any PR» — без ратификации не начинаю.
2. **Без fallback на proto 1.** Новый клиент против старого core получает
   `unsupported_proto` → upgrade-hint и выход (не деградация в one-shot без From→To).
   Почему: образ шипится парой (console пинится в `mcp/Dockerfile.wallet`), рассинхрон —
   только у руками собравшего; fallback = второй кодовый путь и матрица тестов под
   сценарий, которого у пользователей нет (правило №5/№6). Контраргумент: dev-удобство.
   Цена ошибки: билд-инструкция «обнови core» вместо тихой деградации — приемлемо.
3. **Lockout закрывает карточку** (не держит её с ошибкой, как сейчас): сервер уже
   fail-closed уронил очередь — открытая карточка «approvable после разлока» была бы
   ложью. Согласны?
4. **Nav-shell без единого дополнительного вида** — каркас (View-enum, таб-бар,
   маршрутизация) без заглушек. Подтвердить, что это правильная граница между «фундамент
   для Этапа 3» и «оверинжиниринг» (правило №5 vs. ратифицированный порядок этапов).
