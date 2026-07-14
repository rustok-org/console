# Console v2 Фаза 2, Этап 7 (ПОСЛЕДНИЙ) — вид Activity + локальный лог исходов

> Статус: **ПРЕДЛОЖЕНИЕ, ждёт Гейта-1.** Кода нет до двух подписей.
> Управляющие: ТЗ §2 (вид «Activity / History: список решений с фильтрами | локальный лог +
> retained outcomes»), §5 Фаза 2 («Локальный лог активности — терминальные исходы переживают
> 60-мин retention ядра»), §9.5 порядок (Этап 7 — последний Фазы 2); канон §3.9 (op готов,
> core PR3 смержен: id/state/tx_hash?/reason?/age_secs, кап 100, newest first, **id = ключ
> дедупа локального лога**, «console derives an absolute time locally (answer arrival − age)
> when writing its local log»), §3.8 (нормативное клиентское правило list-first).
> Сервер-сторона ГОТОВА (core#93) — этот этап чисто console.
> Команда: Капитан/Инженер/Ревьюер; связь только через Капитана.

## Цель — один абзац

Console-сторона activity, закрывающая Фазу 2: четвёртый вид **Activity/History** (клавиша
`h`) — список терминальных исходов с фильтром по состоянию; клиентская часть op'а
(`Request::Activity` + парс §3.9); **локальный журнал** (JSONL на персистентном томе
`/data`) — история глубже 60-мин retention ядра, куда richly пишется каждое решение в
момент принятия (to/amount из живой карточки) и bedно доливаются server-only исходы
(экспайры, lockout-лавины, чужие соединения) из activity-ответов; возврат
`format::short_addr` — ТОЛЬКО для дисплей-строк этого списка (ТЗ §4.1).

## Замер blast radius (сделан ДО спеки, console main `1b7a16e`, 184+19+1)

1. **Компилятор-форс** от `View::Activity`: render-диспетч (ui.rs:34), три view-блока
   `map_key` (main.rs:316-348 — по 1 строке `h` в каждый + новый блок Activity).
   Существующие таб-тесты — substring-ассерты (ui.rs:1701, :1718) — 4-ю вкладку
   ПЕРЕЖИВАЮТ (/check-4): ожидается ноль правок существующих тестов.
2. **protocol.rs — аддитивно**: `Request::Activity` (serde-tag, парс существующих op не
   меняется) + `OutcomeEntry` + `parse_activity` — зеркало `parse_positions` (:554), но БЕЗ
   `wallet_locked`-ветки: §3.9 фиксирует словарь `unauthorized | protocol_error`, оба на
   корректной authed proto-2 сессии невозможны → любые `ok:false` = `Unexpected` → Fatal
   (класс fail-closed, тот же что `apply_resolve` на Unauthorized).
3. **transport.rs — аддитивно**: `Request::Activity`, `Reply::Activity(Vec<OutcomeEntry>)`,
   2 арма worker-диспетча (:287-305 паттерн Positions).
4. **app.rs — поведенческое ядро**: `ReadOp::Activity` + правка гейта `dispatch_read_op`
   (:901-929, сейчас только `View::Dashboard`); `Reply::Activity`-арм; history-модель
   (`Vec<HistoryEntry>` + фильтр + дренажи/пуши). **`Decision`/`decision_line` НЕ трогаются**
   (ADR-поверхность инварианта #7) — история идёт отдельным дренажем.
5. **main.rs**: ключи, файл-I/O лога, дренажи в цикле (:185-194 — рядом с take_decision).
6. **format.rs**: возврат `short_addr` (убран в Фазе 1 как dead code — бэклог-пункт «вернуть
   в Фазе 2 только для дисплей-списков»).
7. Существующие тесты: правятся ТОЛЬКО tab_line-ассерты; 5 тест-хелперов с явным view,
   scheduler-тесты Этапа 5 (Dashboard-ротация), decision_line-тесты — без правок (критерий 5).

## Ключевые факты (проверены по коду)

- `apply_resolve` держит живую карточку в момент терминального ответа (`c.card` до
  `confirm = None`, app.rs:1056-1084) → id/to/amount_wei/chain_id доступны для rich-записи.
- Модель чистая: ни wall-clock, ни ФС (урок Этапа 2) → все timestamps и файл — в main.rs.
- `/data` — персистентный том wallet-образа (mcp/Dockerfile.wallet: `VOLUME /data`,
  `ENV RUSTOK_DATA_DIR=/data`, chown rustok; `docker exec` наследует ENV и uid образа).
- Транспорт-кап 64 KiB на строку ответа; сервер капнул 100 исходов ≈ 16 KiB — проходит.
- `on_open` уже гейтит `view != Queue` (:777) — Msg::Open из Activity структурно мёртв;
  `on_view` уже отказывает при карточке/get-на-проводе/parked-Get (:600) — Activity
  недостижим при открытой карточке теми же существующими гейтами.
- MoveUp/Down двигают queue-selected независимо от view — существующее безвредное
  поведение Dashboard/Receive, для Activity не меняется (правило №6).

## Дизайн — предрешённые решения

1. **Пайплайн истории: main.rs владеет файлом, часами и dedup-set; модель — чистый
   `Vec<HistoryEntry>` + фильтр.** Три потока:
   - *Загрузка*: main читает JSONL → `model.set_history(entries)` до цикла; dedup-set = id
     загруженных.
   - *Решение (rich)*: `apply_resolve` при терминальном исходе кладёт запись (id, state,
     to **verbatim-строкой как хранение** — рендер сокращает, amount_wei, chain_id,
     tx_hash/reason) в дренаж `take_history_entry()` — БЕЗ unix (у модели нет часов);
     main: unix = now → append в файл → `model.push_history(entry)` **в том же
     проходе цикла, до рендера** (порядок нормативен). Файл отказал → push всё равно
     (session-only деградация, футер-нота).
   - *Activity-реплай (server-only)*: `apply_activity` складывает server-исходы в дренаж
     `take_server_outcomes()`; main: id уже в dedup-set → **fill-missing merge в памяти**
     (/check-2: `ResolveOutcome::AlreadyResolved{state}` несёт только слово — чужое
     соединение исполнило item, наша rich-запись без tx_hash; пустые tx_hash/reason
     добираются из server-исхода через `model.fill_history_detail(id, …)`, присутствующие
     поля НЕ перезаписываются; файл append-only — корректива в файл не пишется, честная
     цена угла); новый id → unix = `now.saturating_sub(age_secs)` → append + push. Возраст
     дрейфует между поллами — дедуп по id это гасит (канон §3.9).
2. **Хранимое слово состояния = протокольное** (`executed|denied|expired|failed`, словарь
   §3.5/§3.9); рендер маппит в человеческие (как Notice::Outcome). Rich-запись конвертирует
   DecisionKind → протокольное слово в одной именованной функции (шов, тест).
3. **Файл**: JSONL append-only, unknown-поля игнорятся (мягкая эволюция без version-поля),
   битая строка → скип + футер-нота «log partially unreadable». **Загрузка дедупит по id**
   (/check-3: две консоли конкурентно аппендят один файл — один server-исход может лечь
   двумя строками; newest wins при загрузке). Путь-каскад:
   `RUSTOK_CONSOLE_LOG` → `$RUSTOK_DATA_DIR/console-activity.jsonl` → ни того ни другого =
   персистентность отключена (session-only, футер-нота). Кап: в памяти/показе newest 500
   **валидных записей** (/check-5: битые строки не считаются и при компакции выбрасываются
   — файл самочинится); компакция при загрузке — >1000 валидных → перезапись newest 500
   дедупнутых (tmp рядом + atomic rename, тот же fs); компакция best-effort (dir не
   writable → скип, чтение работает).
4. **Scheduler**: `ReadOp::Activity` на том же list-first слоте. `dispatch_read_op`:
   `View::Dashboard` → чередование Context/Positions (НЕ меняется); `View::Activity` →
   Activity-op; Queue/Receive → None. Та же `STALE_TICKS`-каденция (~30 c; store-чтение
   дешевле on-chain, но один канал и один слот — консистентность дороже частоты). Вход в
   Activity метит стейл (зеркало Dashboard-ветки `on_view` :610). Все существующие гейты
   слота (flush_pending первым, тройная чистота, только из on_reply(List)) — без изменений.
5. **Вид Activity**: строки newest-first (сортировка по unix desc, **вторичный ключ id
   asc** — lockout-пачка даёт равные unix, детерминизм зеркалит серверную семантику PR3;
   /check-6; модель сортирует при push/set); формат строки: `<возраст> · <исход> · <сумма ETH> → <short_addr(to)>` +
   `tx 0x…кор.` у executed; server-only запись (без to/amount): `<возраст> · <исход> ·
   (details not recorded)` — колонки не разъезжаются, одна честная форма. Возраст из
   `render(_, now_unix)` (уже в сигнатуре) минус unix записи. Фильтр `f` циклит
   All→Executed→Denied→Expired→Failed (модельное поле, живёт только в Activity-блоке
   map_key); активный фильтр — в шапке списка. Пустые состояния: «no activity yet» /
   «no <state> outcomes (filter: f)». Overflow-бюджет — урок Этапа 5: budget один раз,
   в цикле `used`-счётчик, резерв маркера «+K more» кроме последней строки. Notice в
   Activity не рендерится (как Receive). Скролла НЕТ (решение №3 ниже).
6. **short_addr** (format.rs): `0x` + первые 6 + `…` + последние 4 значащих; юнит-тесты
   (длина/крайние). Используется ТОЛЬКО в render_activity — ни на одной
   подписывающей/одобряющей поверхности (ТЗ §4.1; карточка/Receive не тронуты).
7. **Ключи**: `h` → Activity из Queue/Dashboard/Receive; из Activity: `a`/Esc → Queue,
   `d` → Dashboard, `r` → Receive, `q` — quit, `f` — фильтр. `f` свободна во всех блоках.

## Скоуп PR — что входит / что ЯВНО не входит

**Входит:** всё выше + тесты каждого куска red→green + DoD-смоук (pty против proto-2
Python-стаба, стаб дополняется ответом activity).

**ЯВНО НЕ входит:**
- core/сервер — ноль изменений (op готов, core#93).
- Канон-док — ноль (клиентское правило уже в §3.8; §3.9 полон).
- **Activity-строка на Dashboard** (ТЗ §2 числит activity в данных Dashboard) — отдельный
  мини-круг: у render_dashboard свой overflow-бюджет (зона блокера Этапа 5), не смешивать.
  [решение №1]
- Скролл истории [решение №3], поиск, экспорт.
- Реакция вида на `Notice` — не меняется.
- Правка Decision/decision_line (ADR #7) — запрещена этим скоупом.

## Затронутые файлы

`console/src/format.rs` (+short_addr) · `protocol.rs` (+Request::Activity, +OutcomeEntry,
+parse_activity) · `transport.rs` (+Request/Reply армы) · `app.rs` (+View::Activity,
+ReadOp::Activity, +history-модель/дренажи, +apply_activity) · `ui.rs` (+render_activity,
tab_line) · `main.rs` (+ключи, +файл-I/O, +дренажи цикла). Тесты — в тех же файлах.

## Критерии приёмки — «PR готов, когда…»

1. **Red→green на каждый кусок**: parse_activity (все 4 состояния verbatim, отсутствие
   tx_hash/reason = None, пустой массив, ok:false → Unexpected); transport-армы;
   scheduler — зеркала тестов Этапа 5 (activity ТОЛЬКО из on_reply(List) при
   view==Activity && stale && чистый слот; тик не рождает; Dashboard-ротация не изменилась
   — существующие тесты без правок); history (rich при решении с to/amount живой карточки;
   server-merge: новый id входит, знакомый — first-write-wins; сортировка unix desc);
   файл-I/O (загрузка, append, компакция >1000→500, битая строка → скип+нота, нет пути →
   session-only+нота); фильтр-цикл; short_addr; вид (снапшоты строк rich/бедной,
   fg-ассерты, пустые состояния, overflow «+K more» на точной границе); tab_line 4 вкладки;
   ключи h/f.
2. **Лог переживает retention**: тест/смоук — записи старше окна сервера остаются в виде
   (из файла), server-окно их не дублирует.
3. **Инварианты §4**: short_addr только в activity-списке; полные адреса
   карточки/Receive/QR не тронуты; fit-гейт карточки не тронут; секретов в
   логе нет (лог не содержит PIN/сид — только публичные поля исхода).
4. **Порядок цикла нормативен**: дренаж → stamp → append → push → рендер в одном проходе.
5. **decision_line байт-в-байт**: существующие decision_line/Decision тесты БЕЗ правок.
6. **Существующие тесты: ожидается НОЛЬ правок** (/check-4: таб-тесты — substring-ассерты,
   ui.rs:1701 `has_line_with`, :1718 `row.find` — 4-я вкладка аппендится и они выживают;
   4 вкладки ≈ 59 колонок из 80). Любая фактическая правка — поимённо с причиной в отчёте.
7. **Гейты — точные команды CI console** (ci.yml:29/39/48/56): `cargo fmt --all --check` ·
   `cargo clippy --all-targets -- -D warnings` · `cargo test` ·
   `cargo deny check advisories licenses bans sources` (/check-1: джоб Advisories/Licenses/
   Bans проверяет все четыре, не только advisories).
8. **DoD-смоук** pty против дополненного стаба: вход → h → вид с историей; решение по
   карточке → строка появляется мгновенно (rich); стаб-исход другого происхождения →
   появляется после полла (бедная строка); лог-файл на диске; **повторный запуск консоли
   подхватывает историю из файла**; f-фильтр работает; stdout чист от JSON в интерактиве.

## Definition of Done

merged · ветка удалена · CI зелёный · smoke-скрипт сохранён в .claude/reports/ · трекер
console/workflow-state «Этап 7 done, Фаза 2 ЗАКРЫТА» · ТЗ §9.5 + §2-таблица отметки ·
human-plan.html освежён (закрытие сессии) · NEXT = релизный поезд §8.

## Решения к ратификации (Гейт-1) — предрешены

1. **Dashboard activity-строка — НЕ в этом круге.** (а) Рекомендация: view-only;
   (б) почему: у render_dashboard свой хрупкий overflow-бюджет (место блокера Этапа 5) —
   отдельный маленький круг после; Этап 7 в формулировке хэндоффа Капитана строки не
   содержит; (в) контраргумент: ТЗ-таблица §2 числит activity в данных Dashboard —
   недобранная буква ТЗ; не перевесил: §8 всё равно релизит Фазу 2 пакетом, строка не
   двигает time-to-value; (г) цена ошибки: один дополнительный мини-круг; (д) переключатель:
   слово Капитана «в этом круге».
2. **Формат/путь лога**: JSONL + каскад RUSTOK_CONSOLE_LOG → $RUSTOK_DATA_DIR/… →
   session-only; кап 500/1000 с best-effort компакцией. (а)-(б): том /data — факт образа,
   JSONL = скип битой строки без потери файла; (в) контраргумент: sqlite надёжнее — не
   перевесил (новая зависимость против append-only журнала, правило №5); (г) цена: формат
   приватен консоли, мигрируем свободно; (д) переключатель: потребность в поиске/экспорте.
3. **Скролла нет**: топ-N в бюджет + честный «+K more»; фильтры дают срез; файл = полная
   история. (в) контраргумент: длинная история не читается с экрана — не перевесил:
   Activity — дисплей-обзор, не аудит-инструмент; (д) переключатель: боль на живом
   использовании → мини-круг «скролл».
4. **Та же каденция STALE_TICKS для activity** (не чаще): один канал, один слот, list-first
   дороже свежести. Переключатель: жалоба на отставание вида.

## Тест-план

Юнит: format (short_addr ×3) · protocol (parse_activity ×5) · transport (армы ×2) ·
app-scheduler (×6: view-гейт, stale-гейт, list-first, тик-не-рождает, вход-метит,
Dashboard-ротация нетронута) · app-history (×6: rich-дренаж, server-дренаж, сортировка,
фильтр-цикл, push-порядок, **два rich-решения в одну секунду → id-tiebreak — осознанная
документированная особенность, НЕ хронология: unix — единственный источник времени у
чистой модели, секундная гранулярность неизбежна; находка Гейта-1**) ·
main-I/O (×5: load/append/compact/битая/нет-пути) ·
ui (×6: снапшоты rich/poor/empty/filter/overflow, tab_line 4 вкладки на 80 колонках —
покрывает /check-7, fg-ассерт) · ключи (×2).
Каждый — red→green (мутация или true-red), пруфы в отчёте. Смоук — критерий 8.

## Замечено, не трогаю

1. ТЗ §2-таблица: Dashboard-данные включают activity — если реш. №1 ратифицировано,
   отметить строку как «Activity-строка Dashboard — бэклог» при правке ТЗ на закрытии.
2. MoveUp/Down двигают невидимый queue-selected на не-Queue видах — существующее,
   безвредное, не трогаю.
3. `console/README`/доки образа не описывают RUSTOK_CONSOLE_LOG — вписать одну строку
   доков при релизном поезде §8 (не этот PR).
