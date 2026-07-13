# Отчёт: Console v2 Фаза 2, Этап 3 — Receive (адрес + QR) + nav-shell

> Инженер → Гейт-2. Ветка `feat/console-v2-phase2-s3-receive` (2 коммита: `16fa03f` qr-модуль,
> `94f5ddb` nav-shell + Receive), НЕ запушено. Спека:
> `.claude/specs/2026-07-13-console-v2-phase2-s3-receive.md` (Гейт-1 две подписи).

## Сделано — по пунктам спеки

1. **Nav-shell** — `View { Queue, Receive }` в `Phase::Watching` (вид вне Watching
   непредставим); `Msg::View`; тройной гейт переключения ровно как ратифицирован:
   `confirm.is_none() && !awaiting_card && !matches!(pending, Some(PendingIntent::Get(_)))`
   (`on_view`, app.rs:531). Заглушек нет — оба вида реальные.
2. **Гвард модели** — `on_open` отвергает `Msg::Open` при `view != Queue` (/check-3);
   map_key не граница, модель граница.
3. **Таб-бар в существующей хедер-строке** (`tab_line`, ui.rs:52): ` Queue·{n} [a] ·
   Receive [r] `, активная вкладка accent+BOLD+REVERSED (канон выделения Фазы 1), счётчик
   pending живёт от list-поллинга из обоих видов. **Геометрия `watch_chunks` не тронута** —
   fit-гейт карточки не мог сместиться, граничные тесты 80×13/80×24 прошли без правок.
4. **Клавиши**: Queue → `r` (+ хинт-строка дополнена `r receive`); Receive → `a`/Esc назад,
   `q` quit, всё остальное мертво (enter/стрелки/y/n/цифры — тест перечислением);
   при открытой карточке `r`/`a` не маршрутизируются (confirm-арм владеет клавишами).
5. **Вид Receive** (`render_receive`, ui.rs:375): label + **полный EIP-55 адрес verbatim**
   (accent_bright, `push_wrapped` — переносится, не режется) + **QR той же строки** (голый
   адрес, ратификация Капитана). QR эластичен: не влезает по высоте ИЛИ ширине → маркер
   «QR hidden…» (/check-2), частичный/завёрнутый QR не рендерится никогда. Деградация:
   `wallet == None` ИЛИ пустой адрес → «wallet context unavailable — no receive address»,
   ни адреса, ни QR (/check-4). Notice — мебель Queue: в Receive не рендерится, слот
   переживает переключение (/check-5, тест).
6. **`src/qr.rs`** — вся работа с крейтом изолирована (3 вызова API): `half_block_rows` =
   encode (ECC M) + полублоки `█▀▄ ` (2 модуль-строки/текст-строка) + тихая зона 4 модуля;
   пустой текст → `None`. Цвета — `theme::qr_style()`: чистые чёрный/белый через
   NO_COLOR-seam (контраст сканера = функция; NO_COLOR → полярность несут символы).
7. **Зависимость** `qrcodegen = "1"` (v1.8.0 в Cargo.lock) — MIT, 0 транзитивных;
   `cargo deny check` → `advisories ok, bans ok, licenses ok, sources ok`.

## Не сделано / отложено

Ничего из скоупа. (Всё из «ЯВНО НЕ входит» — не тронуто: protocol.rs / transport.rs /
docs/APPROVER-PROTOCOL.md — диф пуст, `Request::`-enum без новых вариантов.)

## Замечено, не трогаю (вне скоупа)

1. `render_receive` не показывает `allowed_chains`/балансы (по спеке); данные уже в Model —
   если Капитан захочет, это отдельная маленькая правка после Этапа 5.
2. Копирование адреса в буфер (OSC 52) — кандидат в бэклог, в ТЗ отсутствует.
3. Смоук-харнесс переиспользует паттерн Этапа 2 (Python-стаб + pty) — если Этапы 5/7 будут
   его повторять, можно оформить постоянным скриптом в репо (решение Капитана, не моё).

## Как тестировал — команды + вывод

**Гейты (все локально, полные команды CI):**
```
cargo test → 156 (lib) + 17 (main) + 1 (tty_gate) зелёные   [было 137+15+1; +21 тест]
cargo fmt --check → чисто
cargo clippy --all-targets -- -D warnings → чисто (Finished, 0 warnings)
cargo deny check → advisories ok, bans ok, licenses ok, sources ok
```

**Red→green (правило №0) — 6 мутаций, каждая роняла ровно свой тест, все откачены:**
| Мутация | Упавший тест |
|---|---|
| полярность `▀`↔`▄` | `the_rendered_qr_round_trips_to_the_exact_module_matrix` |
| QUIET_ZONE 4→2 | `a_four_module_quiet_zone_surrounds_the_code` + `…version_3…` (2 шт) |
| `confirm: None` → `confirm: _` в on_view | `a_view_switch_is_refused_while_a_card_is_open` |
| убран `awaiting_card` из гейта | `a_view_switch_is_refused_while_a_get_is_on_the_wire` |
| убран parked-Get из гейта | `a_view_switch_is_refused_while_a_get_is_parked` (находка Гейта-1 — тест по точному сценарию Ревьюера) |
| убран view-гвард on_open | `open_is_dead_on_the_receive_view` |

**DoD-смоук** (реальный бинарь в pty 80×24 против Python-стаба proto 2,
`scratchpad/smoke_receive.py`): **9/9 PASS** — таб-бар на очереди · тайтл Receive · полный
адрес verbatim · полублоки на экране · truecolor-escapes `38;2;0;0;0` + `48;2;255;255;255`
(чёрным-по-белому) · возврат `a` в очередь · exit-код 6 (aborted) · stdout чист (0 байт,
решений не было) · ровно 15 ink-строк QR версии 3 (по CUP-атрибуции строк).

**Пересмотр замеренных точек blast radius — «был → стал»:**
| Точка | Было | Стало |
|---|---|---|
| ui.rs:172 хедер | `" Pending approvals: {n}"` | `tab_line(View::Queue, n)` |
| ui.rs:1301 ассерт | `contains("Pending approvals")` | `contains("Queue·")` |
| app.rs:748 конструктор | 4 поля | + `view: View::Queue` |
| main.rs:366 хелпер | 4 поля | + `view: View::Queue` (+ новый `receiving()`) |
| ui.rs:26 деструктуринг | 4 поля | + `view` → match |
| ui.rs:212 хинт | `↑/↓ select · enter open · q quit` | + `r receive` |

Прогноз «остальное аддитивно» подтвердился: ни один другой существующий тест не правился.

## Отклонения от плана и почему

1. **Тест-план говорил «QR 19 строк» в снапшоте — ассерт написан как «15 ink-строк + нет
   маркера»**: из 19 текст-строк QR верхние/нижние 2 — чистая тихая зона (пробелы), блоков
   не несут. Структурные 19×37 проверяются в qr.rs; ui-уровень проверяет «код целиком, без
   клипа» через 15 чернильных строк. Первый вариант ассерта (19) честно упал и был
   исправлен как ошибка теста, не кода.
2. Деградационное сообщение Receive покрашено `high_risk_style` (янтарь) — спека цвет не
   фиксировала; выбран как «внимание, не тревога». Дешёво поменять, если Ревьюер возразит.

## Вопросы Ревьюеру

1. Тройной гейт `on_view` читает `self.awaiting_card`/`self.pending` ДО матча по
   `Phase::Watching` — при не-Watching фазах это чтение безвредно (оба поля осмысленны
   только в Watching), но ранний return при parked-Get формально сработает и в Authing
   (недостижимо: pending=Get паркуется только из Watching). Считаешь ли нужным перенести
   проверку внутрь Watching-арма для чистоты?
2. `qr_style()` кладёт чистые `#000`/`#FFF` через `role()` — под NO_COLOR оба падают в
   `Reset` (полярность на символах). Устраивает ли такая деградация, или хочешь явный
   комментарий-тест на NO_COLOR-ветку?

## DoD-остаток (не мой ключ)

Скан QR реальным телефоном (декодеров на хосте нет): запустить
`python3 <scratchpad>/smoke_receive.py` нельзя интерактивно — для живого прогона:
`RUSTOK_APPROVE_SOCK=… target/debug/rustok-console` против стаба, либо проще — Капитан
снимает телефоном экран Receive при ручном прогоне; ожидание: сканер выдаёт байт-в-байт
`0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb` (адрес стаба).
