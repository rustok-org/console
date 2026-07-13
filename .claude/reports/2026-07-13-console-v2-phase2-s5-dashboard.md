# Отчёт: Console v2 Фаза 2, Этап 5 — Dashboard + read-op шедулер

> Инженер → Гейт-2. Ветка `feat/console-v2-phase2-s5-dashboard` (коммит `feat(app,ui)…`),
> НЕ запушено. Спека: `.claude/specs/2026-07-13-console-v2-phase2-s5-dashboard.md`
> (Гейт-1 две подписи; №1 home=Dashboard ратифицирован Капитаном).

## Сделано — по пунктам спеки

1. **protocol.rs**: `Request::Positions`, клиентский `Position` (9 полей, `extra:
   BTreeMap` — рендерим-не-парсим; `protocol` оставлен строкой: неизвестный будущий
   протокол рендерится как есть, не валит парс), `PositionsOutcome`, `parse_positions`
   (ok/пустой список валиден/wallet_locked/unauthorized+protocol_error → fail-closed/мусор
   → Malformed). 6 тестов, вкл. verbatim «∞»/«80%» и `{"op":"positions"}`-энкод.
2. **transport.rs**: прокладка Request/Reply (паттерн Context, механически).
3. **app.rs — шедулер (ядро)**: `View::Dashboard` (+home после auth — ратификация №1);
   `Positions`-тристейт (`NotYet | Loaded | Unavailable`); `read_age` (тики; **сброс в
   точке диспетча** — дизайн-находка кодинга: сброс в apply_* заставил бы первые positions
   ждать ~30 c после auth-context; born-stale дефолт даёт первый показ сразу после первого
   list-reply, как требовала спека); `next_read` (чередование Context/Positions);
   `context_stale` (wallet_locked-рефреш хранит данные + метит; успех снимает).
   **Политика list-first дословно**: read-op только из `on_reply(List)`, никогда из тика;
   `flush_pending()` первым (/check-1); тройная чистота слота; вход на Dashboard метит
   стейл (`max(STALE_TICKS)` — не сбрасывает более старый возраст). Док-коммент
   `dispatch_read_op` несёт честную границу гарантии (формулировка Ревьюера с Гейта-1).
4. **ui.rs**: третья вкладка ` Dashboard [d] ` (первая в ряду — home); `render_dashboard`:
   Waiting (янтарь при pending>0) → Balance (per-chain `wei_to_eth`, «no balances
   reported» ≠ «balance unavailable», staleness-нота) → Positions (тристейт-тексты;
   строка: protocol · balance_formatted symbol name — extra-пары verbatim; **адресов
   нет** — №4; overflow → «+N more — terminal too small»).
5. **main.rs**: `d` из Queue/Receive; Dashboard-арм: `a`/Esc/`r`/`q`, остальное мертво
   (перечислением, вкл. Enter — карточка за экраном не откроется; модельный view-гвард
   on_open с Этапа 3 это дублирует).
6. **Хелперы**: ровно 5 (app.rs `watching`, ui.rs ×3, main.rs `confirming_at`) получили
   явный `Msg::View(View::Queue)` — **0 существующих тестов переписано** (прогноз спеки
   сошёлся).

## Не сделано / отложено

Ничего из скоупа. (Секция активности на дашборде — Этап 7, отсутствует, не заглушена.)

## Замечено, не трогаю

1. `poll_round`-хелпер тестов ассертит «тик → всегда List» в каждом прогоне — инвариант
   (а) проверяется структурно во всех шедулер-тестах, не одним.
2. Red-тест порядка «flush > read-op» реализован ближайшей исполнимой формой (Queue +
   parked get → после List уходит Get): состояние «parked get + активный Dashboard»
   сегодня недостижимо — гейт переключения отвергает таб при parked get (Этап 3). Порядок
   в коде закреплён + задокументирован; наблюдаемым он станет, только если появится вид,
   паркующий интенты. Ревьюеру на подтверждение.

## Как тестировал — команды + вывод

```
cargo test → 180 (lib) + 19 (main) + 1 (tty_gate) зелёные   [было 159+17+1; +23 теста]
cargo fmt --check → чисто · clippy --all-targets -D warnings → чисто · deny → ok×4
```

**Red→green — 7 мутаций шедулера, каждая роняла ровно свой тест, все откачены:**
| Мутация | Упавший тест |
|---|---|
| list-first снят (`was_list`→false) | `the_first_list_reply_…_dispatches_positions` |
| view-гейт снят | `a_read_op_is_refused_off_the_dashboard` |
| стейлнес-гейт снят | `read_ops_wait_out_the_staleness_window` |
| вход-на-дашборд не метит стейл | `entering_the_dashboard_marks_the_data_stale` |
| `context_stale` не ставится | `a_failed_balance_refresh_keeps_the_data_and_flags_it` |
| home снова Queue | `the_home_view_after_auth_is_the_dashboard` |
| read-op рождается из тика | `the_first_list_reply_…` (через poll_round-ассерт) |

(Каденция (в) и чередование (г) — два разных ассерта одного теста
`read_ops_wait_out_the_staleness_window`; мутация стейлнеса доказана, мутацию чередования
покрывает финальный ассерт того же теста.)

**DoD-смоук** (pty 80×24, стаб proto 2 с Aave-фикстурой из канона §3.8,
`scratchpad/smoke_dashboard.py`): **10/10 PASS** — home=Dashboard после PIN · три вкладки
с клавишами · Waiting-блок · «chain 1 0.01 ETH» · «aave_v3 1000 USD» · «∞» и «80%»
verbatim на экране · `a`→Queue · `d`→обратно · exit 6 · stdout чист.

## Отклонения от плана и почему

1. **Семантика `read_age`: сброс в точке диспетча, не в apply_*** (дизайн-уточнение,
   в плюс): сброс на auth-context-ответе заставил бы первые positions ждать полное окно
   (~30 c) после входа — против «первый показ сразу» из спеки. Born-stale дефолт +
   сброс-на-диспетч дают заявленную каденцию (каждый op ~раз в 60 c, офсет 30 c) и
   немедленный первый фетч.
2. Мутация (б) спеки («read-op при открытой карточке») не исполнима буквально: карточка
   существует только в Queue-виде, а view-гейт диспетча (мутация M2) покрывает то же
   условие строже. Задокументировано выше.

## Вопросы Ревьюеру

1. «Замечено» п.2 — согласен ли, что недостижимость «parked get + Dashboard» делает
   red-тест порядка структурно невозможным, и закрепление порядка кодом+комментарием
   достаточно?
2. `render_dashboard` overflow-бюджет считает `budget + if remaining == 1 { 1 } else { 0 }`
   (последняя позиция может занять строку маркера) — читабельность vs точность; есть ли
   более чистая формулировка на твой вкус?
