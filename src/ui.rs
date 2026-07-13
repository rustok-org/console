//! Rendering — a pure function of the [`Model`], immediate-mode (rebuilt every
//! frame). The console shows the core's values **verbatim** (`AGENTS.md` #1): the
//! card renders the fields as received, adding no interpretation. The PIN is shown
//! only as a row of dots — never the digits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{AuthError, Confirm, DecisionKind, Model, Notice, Phase, ResolveError, View};
use crate::protocol::{Card, Summary};
use crate::{format, qr, theme};

/// Render the whole screen for the current model.
///
/// `now_unix` is the wall clock, passed in rather than read here: the [`Model`]
/// stays a pure function of its messages, and the countdown stays testable.
pub fn render(frame: &mut Frame, model: &Model, now_unix: u64) {
    match model.phase() {
        Phase::Connecting => {
            render_centered(frame, "Connecting to the wallet…");
        }
        Phase::Authing { pin, error } => render_auth(frame, pin.len(), error.as_ref()),
        Phase::Watching {
            items,
            selected,
            confirm,
            notice,
            view,
        } => match view {
            View::Queue => render_watch(
                frame,
                items,
                *selected,
                confirm.as_deref(),
                notice.as_ref(),
                model.wallet_address(),
                now_unix,
            ),
            View::Receive => render_receive(frame, items.len(), model.wallet_address()),
        },
        Phase::Fatal(err) => render_centered(frame, &err.to_string()),
    }
}

/// The nav-shell tab bar — one line, both registered views with their keys,
/// the active one highlighted the way the queue highlights its selection
/// (accent + reversed). The pending count rides the Queue tab so a human on
/// Receive still sees work arriving.
fn tab_line(active: View, pending: usize) -> Line<'static> {
    let tab = |text: String, is_active: bool| {
        if is_active {
            Span::styled(
                text,
                Style::new()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED),
            )
        } else {
            Span::styled(text, theme::label_style())
        }
    };
    Line::from(vec![
        Span::raw(" "),
        tab(format!(" Queue·{pending} [a] "), active == View::Queue),
        Span::raw(" "),
        tab(" Receive [r] ".to_owned(), active == View::Receive),
    ])
}

/// The transient notice line — the resident console's replacement for the old
/// exit-with-outcome screen. Styled by weight: a lockout in the high-risk
/// amber, a decision outcome in its semantic color, a plain note unstyled.
fn notice_line(notice: &Notice) -> Line<'static> {
    match notice {
        Notice::Locked { retry_after_s } => {
            let text = match retry_after_s {
                // Only *pending* items are denied by the fail-closed drop — an
                // item already executing is untouched (protocol §4), so this
                // text must not bury a live signature.
                Some(s) => format!("PIN locked — pending items were denied. Retry in ~{s}s."),
                None => "PIN locked — pending items were denied.".to_owned(),
            };
            Line::from(Span::styled(text, theme::high_risk_style()))
        }
        Notice::Outcome { kind, detail } => {
            let (headline, color) = match kind {
                DecisionKind::Approved => ("APPROVED", theme::approve()),
                DecisionKind::Rejected => ("REJECTED", theme::reject()),
                DecisionKind::Expired => ("EXPIRED", theme::high_risk()),
                DecisionKind::Failed => ("FAILED", theme::reject()),
            };
            let text = match detail {
                Some(d) => format!("{headline} — {d}"),
                None => headline.to_owned(),
            };
            Line::from(Span::styled(
                text,
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ))
        }
        Notice::Note(text) => Line::from(text.clone()),
    }
}

/// A framed panel in the brand palette: soft border, accent title. One helper so
/// every screen frames the same way.
fn themed_block(title: &str) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().fg(theme::frame()))
        .title(Line::from(Span::styled(
            title.to_owned(),
            theme::heading_style(),
        )))
}

fn render_centered(frame: &mut Frame, message: &str) {
    let paragraph = Paragraph::new(message).block(themed_block(" Rustok Console "));
    frame.render_widget(paragraph, frame.area());
}

fn render_auth(frame: &mut Frame, pin_len: usize, error: Option<&AuthError>) {
    let mut lines = vec![
        Line::from("Enter your wallet PIN, then press Enter."),
        Line::from(""),
        // Only the count is shown — never the digits.
        Line::from(Span::styled(
            "●".repeat(pin_len),
            Style::new().add_modifier(Modifier::BOLD),
        )),
    ];
    if let Some(err) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            auth_error_text(err),
            Style::new().fg(theme::reject()),
        )));
    }
    let paragraph = Paragraph::new(lines).block(themed_block(" Unlock "));
    frame.render_widget(paragraph, frame.area());
}

fn auth_error_text(err: &AuthError) -> String {
    match err {
        AuthError::BadPin(left) => format!("Wrong PIN — {left} attempt(s) left."),
        AuthError::Locked(secs) => format!("Locked out. Try again in {secs}s."),
        AuthError::NotSet => "This wallet has no PIN set (run set-pin).".to_owned(),
        AuthError::Unavailable => "PIN check unavailable — try again.".to_owned(),
    }
}

/// Split the watch screen. One function for the renderer AND for
/// [`priority_fields_fit`], so the approve gate can never disagree with the
/// layout actually drawn.
///
/// The note's row is claimed only when there is a note. An always-reserved row
/// would take its space from the card, and the card is the one thing on this
/// screen whose priority fields must never leave the screen (`AGENTS.md` #1).
///
/// While a confirmation is open the card is the decision surface: the queue
/// collapses to a single-item strip (the List keeps the selection in view)
/// and the card takes every remaining row. Splitting the height evenly would
/// starve the card of the rows its risk warnings and PIN prompt need on a
/// 24-row terminal.
fn watch_chunks(
    area: ratatui::layout::Rect,
    confirm_open: bool,
    has_note: bool,
) -> std::rc::Rc<[ratatui::layout::Rect]> {
    let queue_rows = if confirm_open {
        Constraint::Length(3) // borders + the selected row
    } else {
        Constraint::Min(3)
    };
    let mut constraints = vec![
        Constraint::Length(1), // header
        queue_rows,            // queue
        Constraint::Min(6),    // card / hint
        Constraint::Length(1), // decision row / navigation hint
    ];
    if has_note {
        constraints.push(Constraint::Length(1)); // transient note
    }
    Layout::vertical(constraints).split(area)
}

fn render_watch(
    frame: &mut Frame,
    items: &[Summary],
    selected: usize,
    confirm: Option<&Confirm>,
    notice: Option<&Notice>,
    wallet: Option<&str>,
    now_unix: u64,
) {
    let chunks = watch_chunks(frame.area(), confirm.is_some(), notice.is_some());

    // The tab bar lives in the header row the layout already had — the card's
    // geometry (and with it `priority_fields_fit`) is untouched by nav-shell.
    frame.render_widget(
        Paragraph::new(tab_line(View::Queue, items.len())),
        chunks[0],
    );

    render_queue(frame, items, selected, chunks[1]);
    render_detail(frame, confirm, wallet, chunks[2]);
    // The same fit the model gates approve on (`priority_fields_fit`), taken
    // from the very chunk the card is drawn into.
    let approve_ok = confirm.is_none_or(|c| card_priority_fits(c, wallet, chunks[2]));
    render_actions(frame, confirm, approve_ok, now_unix, chunks[3]);

    if let Some(notice) = notice {
        frame.render_widget(Paragraph::new(notice_line(notice)), chunks[4]);
    }
}

/// Seconds left before the open card's deadline.
///
/// Saturating on purpose: a deadline already in the past reads as `0`, never as a
/// wrapped-around eternity. An unreadable clock reaches us as `u64::MAX` (see
/// `main::now_unix`) and lands here as `0` too — a broken clock can never hand an
/// approval more time.
fn seconds_left(not_after_unix: u64, now_unix: u64) -> u64 {
    not_after_unix.saturating_sub(now_unix)
}

/// The decision row.
///
/// The countdown rides the **Reject** button and nothing else on this screen moves
/// (`AGENTS.md` #5). Reject is drawn as the focused button — reversed and bold —
/// because it is what happens if the human does nothing; Approve is a quiet outline
/// that has to be chosen. The copy says so out loud: `auto in 27s`.
fn render_actions(
    frame: &mut Frame,
    confirm: Option<&Confirm>,
    approve_ok: bool,
    now_unix: u64,
    area: ratatui::layout::Rect,
) {
    let Some(confirm) = confirm else {
        frame.render_widget(
            Paragraph::new("  ↑/↓ select · enter open · r receive · q quit"),
            area,
        );
        return;
    };
    if confirm.is_resolving() {
        // The decision is on the wire and the buttons are gone with it, so a second
        // press cannot be mistaken for a second decision.
        frame.render_widget(Paragraph::new("  Sending your decision…"), area);
        return;
    }

    // The PIN prompt owns Enter, so Enter — not `y` — is what approves while it is up.
    let approve_key = if confirm.pin_len().is_some() {
        "enter"
    } else {
        "y"
    };
    let reject_key = if confirm.pin_len().is_some() {
        "esc"
    } else {
        "n / esc"
    };
    let left = seconds_left(confirm.card().not_after_unix, now_unix);
    // Reject is the focused, default-deny button (reversed + bold, tinted red) —
    // it is what happens if the human does nothing. Approve is the quiet teal
    // choice that has to be made.
    let reject_button = Span::styled(
        format!("[ {reject_key}  Reject · auto in {left}s ]"),
        Style::new()
            .fg(theme::reject())
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
    );

    // No live Approve button on a card the human cannot read (the model refuses
    // the key too — `priority_fields_fit`). Reject stays: default-deny may
    // never depend on the terminal being big enough (`AGENTS.md` #5).
    let row = if approve_ok {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("[ {approve_key}  Approve ]"),
                Style::new().fg(theme::approve()),
            ),
            Span::raw("    "),
            reject_button,
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "  approve disabled — terminal too small    ",
                Style::new().fg(theme::high_risk()),
            ),
            reject_button,
        ])
    };
    frame.render_widget(Paragraph::new(row), area);
}

fn render_queue(
    frame: &mut Frame,
    items: &[Summary],
    selected: usize,
    area: ratatui::layout::Rect,
) {
    let block = themed_block(" Queue ");
    if items.is_empty() {
        let empty = Paragraph::new("Queue is empty — waiting for approval requests…").block(block);
        frame.render_widget(empty, area);
        return;
    }
    let rows: Vec<ListItem> = items
        .iter()
        .map(|s| {
            let flag = if s.high_risk { "⚠ " } else { "  " };
            let text = format!(
                "{flag}{kind:5} {to}  {amount} wei",
                kind = kind_word(s),
                to = s.to,
                amount = s.amount_wei
            );
            // A high-risk item is amber even in the list, so danger reads before
            // the card is opened.
            if s.high_risk {
                ListItem::new(Span::styled(text, Style::new().fg(theme::high_risk())))
            } else {
                ListItem::new(text)
            }
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(selected.min(items.len().saturating_sub(1))));
    let list = List::new(rows)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::new()
                .fg(theme::accent())
                .add_modifier(Modifier::REVERSED),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn kind_word(s: &Summary) -> &'static str {
    match s.kind {
        crate::protocol::Kind::Send => "send",
        crate::protocol::Kind::Call => "call",
    }
}

/// The Receive view: the wallet's own address in FULL (verbatim EIP-55 from
/// `context` — the string a sender must see) and a QR of **exactly that
/// string** (bare address, no URI scheme — Gate-1 ratification). Pure
/// display: signs nothing, sends nothing.
///
/// The address is the priority element: it renders wrapped, and when even it
/// cannot fit, the screen says so with a banner — never a silent cut (the
/// card's TOO SMALL pattern, [`render_detail`]). The QR is the elastic one:
/// when its rows do not fit the remaining area — too few rows OR too few
/// columns (a `Wrap`-folded QR would still look scannable and scan as
/// garbage) — an explicit marker takes its place, the raw_data honesty
/// pattern.
///
/// Degraded context (`wallet_locked`, an old server — `None` here) and an
/// empty address (`parse_context` rejects a missing one, not an empty one)
/// show "no receive address": a QR of nothing must never be fabricated.
fn render_receive(frame: &mut Frame, pending: usize, wallet: Option<&str>) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());
    frame.render_widget(Paragraph::new(tab_line(View::Receive, pending)), chunks[0]);

    let block = themed_block(" Receive ");
    let inner = block.inner(chunks[1]);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(addr) = wallet.filter(|a| !a.is_empty()) {
        push_wrapped(
            &mut lines,
            width,
            "your address".to_owned(),
            theme::label_style(),
        );
        push_wrapped(
            &mut lines,
            width,
            addr.to_owned(),
            Style::new().fg(theme::accent_bright()),
        );
        match qr::half_block_rows(addr) {
            // `first()`, not `[0]`: a non-empty row set is the encoder
            // crate's invariant, not this module's contract — an empty one
            // degrades to the marker instead of panicking.
            Some(rows)
                if rows.first().is_some_and(|r| r.chars().count() <= width)
                    && lines.len() + rows.len() <= height =>
            {
                lines.extend(
                    rows.into_iter()
                        .map(|row| Line::from(Span::styled(row, theme::qr_style()))),
                );
            }
            _ => {
                // No wrapped, clipped or fabricated QR — say so instead.
                // ~24 rows: tab bar + 2 borders + label + address + 19 QR.
                push_wrapped(
                    &mut lines,
                    width,
                    "QR hidden — terminal too small (needs ~24 rows × 39 cols); \
                     the address above is complete"
                        .to_owned(),
                    theme::label_style(),
                );
            }
        }
    } else {
        push_wrapped(
            &mut lines,
            width,
            "wallet context unavailable — no receive address".to_owned(),
            theme::high_risk_style(),
        );
    }

    if lines.len() > height {
        // Even the pre-QR lines overflow: the address is about to be cut,
        // and it must never be cut in silence — a copied half-address is the
        // receive surface's own poisoning vector. The banner takes the one
        // row guaranteed visible when rows clip (same as `render_detail`).
        let mut banner = Vec::new();
        push_wrapped(
            &mut banner,
            width,
            "TERMINAL TOO SMALL — the address below is cut; resize to read it in full".to_owned(),
            Style::new().add_modifier(Modifier::BOLD),
        );
        banner.append(&mut lines);
        lines = banner;
    }
    frame.render_widget(Paragraph::new(lines).block(block), chunks[1]);
}

/// The card's priority lines — every field except `raw_data` — pre-wrapped to
/// `width` display cells, so one logical line is one visual row and the height
/// arithmetic downstream is exact. One source for the renderer AND for
/// [`priority_fields_fit`]: the approve gate can never disagree with what is
/// actually drawn.
fn priority_lines(confirm: &Confirm, from: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let card: &Card = confirm.card();

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Native value, human-first: `10000000000000000` reads as `0.01 ETH`. A token
    // op sends `0` native wei with the real amount in `decoded_call` (below), so a
    // zero native value is NOT headlined as "0 ETH" — the decoded call carries the
    // movement (e.g. an unlimited approval must not look like it moves nothing).
    if !format::is_zero_wei(&card.amount_wei) {
        push_wrapped(
            &mut lines,
            width,
            format!("amount  {}", format::wei_to_eth(&card.amount_wei)),
            theme::heading_style(),
        );
    }
    // Addresses are shown in FULL, verbatim — never shortened. A clear-signing
    // card is where the human verifies exactly WHO receives funds; a `0x1234…abcd`
    // ellipsis would hide an address-poisoning look-alike (`AGENTS.md` #1).
    //
    // With the wallet's own address known (`context`, proto 2) the card reads
    // as a two-block From→To flow — stacked vertically, INSIDE the priority
    // lines, so the fit gate counts every row of it (a side-by-side layout
    // would live outside `priority_lines` and the gate could not see it).
    // Without it (the read-op degraded) the card falls back to the To-only
    // layout — the From block is display-only and never gates approve.
    if let Some(from) = from {
        push_wrapped(
            &mut lines,
            width,
            "from  your wallet".to_owned(),
            theme::label_style(),
        );
        push_wrapped(
            &mut lines,
            width,
            format!("      {from}"),
            Style::new().fg(theme::ink()),
        );
        push_wrapped(&mut lines, width, "  ↓".to_owned(), theme::label_style());
    }
    push_wrapped(
        &mut lines,
        width,
        format!("to  {}", card.to),
        Style::new().fg(theme::accent_bright()),
    );
    push_wrapped(
        &mut lines,
        width,
        format!("chain  {}", card.chain_id),
        theme::label_style(),
    );
    if card.high_risk {
        push_wrapped(
            &mut lines,
            width,
            format!("⚠ HIGH RISK  {}", card.high_risk_reasons.join(", ")),
            theme::high_risk_style(),
        );
    }
    // A plain send has nothing to decode — the old "decoded_call: (none)" line was
    // noise, so it is dropped. A contract call keeps every decoded field: WHO is
    // authorized (spender/operator/from/to/token) is the point of the card.
    if let Some(dc) = &card.decoded_call {
        // Label convention matches `push_opt` (`decoded_call.<field>`) so the method
        // reads as one of the decoded fields, just emphasized.
        push_wrapped(
            &mut lines,
            width,
            format!("decoded_call.method: {}", dc.method),
            theme::heading_style(),
        );
        push_opt(&mut lines, width, "spender", dc.spender.as_deref());
        push_opt(&mut lines, width, "operator", dc.operator.as_deref());
        push_opt(&mut lines, width, "from", dc.from.as_deref());
        push_opt(&mut lines, width, "to", dc.to.as_deref());
        push_opt(&mut lines, width, "token", dc.token.as_deref());
        push_opt(&mut lines, width, "amount", dc.amount.as_deref());
        push_opt(&mut lines, width, "deadline", dc.deadline.as_deref());
        if dc.is_unlimited == Some(true) {
            push_wrapped(
                &mut lines,
                width,
                "amount  UNLIMITED".to_owned(),
                theme::high_risk_style(),
            );
        }
    }
    if let Some(pin_len) = confirm.pin_len() {
        lines.push(Line::from(""));
        push_wrapped(
            &mut lines,
            width,
            "High-risk approval — enter your PIN:".to_owned(),
            theme::value_style(),
        );
        // Only the count is shown — never the digits.
        push_wrapped(
            &mut lines,
            width,
            "●".repeat(pin_len),
            theme::high_risk_style(),
        );
    }
    if let Some(err) = confirm.error() {
        lines.push(Line::from(""));
        push_wrapped(
            &mut lines,
            width,
            resolve_error_text(err),
            Style::new().fg(theme::reject()),
        );
    }
    lines
}

/// Whether the card's priority lines fit its inner area.
fn card_priority_fits(confirm: &Confirm, from: Option<&str>, area: ratatui::layout::Rect) -> bool {
    let inner = Block::bordered().inner(area);
    priority_lines(confirm, from, usize::from(inner.width)).len() <= usize::from(inner.height)
}

/// The approve gate: can a `width`×`height` terminal show every priority field
/// of the open card? Runs the same layout ([`watch_chunks`]) and the same line
/// pre-wrap ([`priority_lines`]) as the renderer, so the gate, the banner and
/// the missing Approve button always agree. The [`Model`] consults this before
/// letting `y` or a PIN submit do anything — a "yes" to a card the human could
/// not read is not a decision (`AGENTS.md` #1).
///
/// `has_note` is `false` by construction: a note and an open confirmation never
/// coexist (`apply_get`/`apply_resolve` set one while clearing the other).
#[must_use]
pub fn priority_fields_fit(confirm: &Confirm, from: Option<&str>, width: u16, height: u16) -> bool {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let chunks = watch_chunks(area, true, false);
    card_priority_fits(confirm, from, chunks[2])
}

/// Render the open confirmation's card — the core's fields **verbatim**, no
/// re-derivation. `None` shows a hint to open one.
///
/// Priority fields (everything except `raw_data`) render first, and
/// `raw_data` — the only elastic element — gets exactly the rows that remain,
/// truncated with an explicit marker when it cannot fit. A long calldata can
/// therefore never push a risk warning or the PIN prompt off the screen. When
/// the priority fields alone cannot fit (a terminal below ~24 rows, or
/// pathological server data), the card says so with a banner and the approve
/// path is gated off ([`priority_fields_fit`]) until the terminal grows.
fn render_detail(
    frame: &mut Frame,
    confirm: Option<&Confirm>,
    from: Option<&str>,
    area: ratatui::layout::Rect,
) {
    let block = themed_block(" Card ");
    let Some(confirm) = confirm else {
        let hint =
            Paragraph::new("Select a request and press enter to see the full card.").block(block);
        frame.render_widget(hint, area);
        return;
    };

    let inner = block.inner(area);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);

    let mut lines = priority_lines(confirm, from, width);
    if lines.len() > height {
        // The card cannot show what the human must read; approve is gated off
        // (`priority_fields_fit` — the model refuses `y` and PIN submits). The
        // banner goes on top: the one row guaranteed visible when rows clip.
        let mut banner = Vec::new();
        push_wrapped(
            &mut banner,
            width,
            "TERMINAL TOO SMALL — approve disabled; resize to read the card (reject works)"
                .to_owned(),
            Style::new().add_modifier(Modifier::BOLD),
        );
        banner.append(&mut lines);
        lines = banner;
    }

    // raw_data comes LAST and absorbs whatever rows the priority fields above
    // left over. Never truncate priority fields; a truncated raw_data says so
    // out loud — a silent clip would be the one lie this screen exists to
    // prevent. Wrap stays on as a backstop only (every line already fits the
    // width); trim: false keeps the exact bytes, including leading space.
    let budget = height.saturating_sub(lines.len());
    push_raw_data(&mut lines, width, budget, &confirm.card().raw_data);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Push `raw_data` into exactly `budget` visual rows. It fits → shown whole.
/// It does not → truncated with a marker naming how much is hidden; the marker
/// lives inside the same budget, so it can never spill onto the priority
/// fields. A zero budget (priority fields alone fill the card — an anomalously
/// long decoded_call) degrades to a marker-only line, clipped by ratatui if
/// even that row has no room; it never panics.
fn push_raw_data(lines: &mut Vec<Line<'static>>, width: usize, budget: usize, raw: &str) {
    use unicode_width::UnicodeWidthStr;

    let total = raw.chars().count();
    if budget == 0 {
        push_wrapped(
            lines,
            width,
            format!("raw_data: (hidden — card too small for {total} chars)"),
            Style::new(),
        );
        return;
    }
    let full_rows = chunk_display_width(&format!("raw_data: {raw}"), width);
    if full_rows.len() <= budget {
        lines.extend(full_rows.into_iter().map(Line::from));
        return;
    }

    // Size the shown prefix by display cells, reserving room for the marker at
    // its widest (both counters as wide as `total`). The digit widths change
    // with `shown`, so verify by chunking and shrink a row's worth at a time —
    // strictly downward, stopping at zero, where the marker alone is pushed.
    let cells = budget.saturating_mul(width.max(1));
    let overhead = truncated_raw_line("", total, total).width();
    let mut shown = prefix_chars_for_cells(raw, cells.saturating_sub(overhead));
    loop {
        let prefix: String = raw.chars().take(shown).collect();
        let candidate = truncated_raw_line(&prefix, shown, total);
        if chunk_display_width(&candidate, width).len() <= budget || shown == 0 {
            push_wrapped(lines, width, candidate, Style::new());
            return;
        }
        shown = shown.saturating_sub(width.max(1));
    }
}

/// The truncated `raw_data` line: head of the value plus an explicit marker.
fn truncated_raw_line(prefix: &str, shown: usize, total: usize) -> String {
    let hidden = total.saturating_sub(shown);
    format!(
        "raw_data: {prefix}… ({total} chars total, {shown} shown, {hidden} not shown — scroll not yet supported)"
    )
}

/// How many leading `chars` of `s` fit within `cells` display cells.
fn prefix_chars_for_cells(s: &str, cells: usize) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut used = 0;
    let mut count = 0;
    for ch in s.chars() {
        used += ch.width().unwrap_or(0);
        if used > cells {
            break;
        }
        count += 1;
    }
    count
}

/// Push `text` as one or more lines, each at most `width` display cells — the
/// pre-wrapping that keeps `render_detail`'s row arithmetic exact.
fn push_wrapped(lines: &mut Vec<Line<'static>>, width: usize, text: String, style: Style) {
    for chunk in chunk_display_width(&text, width) {
        lines.push(Line::from(Span::styled(chunk, style)));
    }
}

/// Split `s` into chunks of at most `width` display cells, never inside a
/// `char`. Measured with the same unicode-width ratatui renders with, so a
/// chunk always fits one terminal row.
fn chunk_display_width(s: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn resolve_error_text(err: &ResolveError) -> String {
    match err {
        ResolveError::PinRequired => "This approval needs your PIN.".to_owned(),
        ResolveError::BadPin(left) => format!("Wrong PIN — {left} attempt(s) left."),
        ResolveError::NotSet => "This wallet has no PIN set (run set-pin).".to_owned(),
        ResolveError::Unavailable => "PIN check unavailable — try again.".to_owned(),
        ResolveError::Busy => "Another approval is executing this request — retry.".to_owned(),
    }
}

fn push_opt(lines: &mut Vec<Line<'static>>, width: usize, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        push_wrapped(
            lines,
            width,
            format!("decoded_call.{key}: {v}"),
            Style::new(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Model, Msg};
    use crate::protocol::{
        AuthOutcome, Card, ContextOutcome, DecodedCall, Kind, Risk, WalletContext,
    };
    use crate::transport::Reply;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A fixed "now" for the countdown tests. Real time never enters the renderer.
    const NOW: u64 = 1_000_000_000;

    /// Render into a fixed grid, returning the screen as rows. Row-level checks
    /// catch a field rendered under the WRONG label (a swap) — which a
    /// whole-screen substring check would miss.
    fn draw_rows_at(model: &Model, w: u16, h: u16, now_unix: u64) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, model, now_unix)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn draw_rows(model: &Model, w: u16, h: u16) -> Vec<String> {
        draw_rows_at(model, w, h, NOW)
    }

    /// Foreground colors on the first rendered row containing `needle`. Lets a test
    /// assert that COLOR — not just text — carries the meaning: a high-risk row is
    /// amber, Approve teal, Reject red. Without this, a swap of `high_risk()` for
    /// `accent()` would render fine and no text-only test would notice.
    fn row_fgs_containing(
        model: &Model,
        w: u16,
        h: u16,
        needle: &str,
    ) -> Vec<ratatui::style::Color> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, model, NOW)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..h {
            let text: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
            if text.contains(needle) {
                return (0..w).filter_map(|x| buffer[(x, y)].style().fg).collect();
            }
        }
        Vec::new()
    }

    /// Flatten to one String for checks that do not care about layout.
    fn draw(model: &Model, w: u16, h: u16) -> String {
        draw_rows(model, w, h).join("\n")
    }

    /// The rendered decision row (the line carrying the buttons).
    fn action_row(rows: &[String]) -> String {
        rows.iter()
            .find(|r| r.contains("Approve"))
            .expect("the decision row must render")
            .clone()
    }

    /// True if some rendered line contains all `fragments` — a label+value
    /// adjacency check, so a swapped field is caught.
    fn has_line_with(rows: &[String], fragments: &[&str]) -> bool {
        rows.iter()
            .any(|row| fragments.iter().all(|f| row.contains(f)))
    }

    fn summary(id: &str, to: &str, amount: &str, high_risk: bool) -> Summary {
        Summary {
            id: id.to_owned(),
            kind: Kind::Call,
            chain_id: 1,
            to: to.to_owned(),
            amount_wei: amount.to_owned(),
            risk: Risk::Safe,
            high_risk,
            not_after_unix: 1,
        }
    }

    /// The wallet's own address in tests — full-length, so the From block
    /// exercises real wrapping.
    const WALLET: &str = "0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb";

    fn to_watching(model: &mut Model, items: Vec<Summary>) {
        // The size report main sends at startup — a standard 80×24 terminal.
        model.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        model.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        model.update(Msg::PinDigit('1'));
        model.update(Msg::PinSubmit);
        model.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
        // the everyday session: the context lands right after auth
        model.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        model.update(Msg::Tick);
        model.update(Msg::Reply(Reply::List(items)));
    }

    /// A session whose `context` answered ok with an EMPTY address string —
    /// distinct from a degraded context: `parse_context` rejects a missing
    /// address but passes `""` through (T2).
    fn to_watching_empty_address(model: &mut Model, items: Vec<Summary>) {
        model.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        model.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        model.update(Msg::PinDigit('1'));
        model.update(Msg::PinSubmit);
        model.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
        model.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: String::new(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        model.update(Msg::Tick);
        model.update(Msg::Reply(Reply::List(items)));
    }

    /// A session whose `context` degraded (`wallet_locked`): the card falls
    /// back to the To-only layout of Phase 1.
    fn to_watching_no_context(model: &mut Model, items: Vec<Summary>) {
        model.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        model.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        model.update(Msg::PinDigit('1'));
        model.update(Msg::PinSubmit);
        model.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
        model.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        model.update(Msg::Tick);
        model.update(Msg::Reply(Reply::List(items)));
    }

    #[test]
    fn auth_screen_masks_the_pin_with_dots_never_the_digits() {
        let mut m = Model::new();
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        for c in "4839".chars() {
            m.update(Msg::PinDigit(c));
        }
        let screen = draw(&m, 60, 12);
        assert!(screen.contains("●●●●"), "four dots for four digits");
        assert!(!screen.contains("4839"), "the digits must never render");
    }

    #[test]
    fn empty_queue_shows_a_waiting_message() {
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        let screen = draw(&m, 80, 20);
        assert!(screen.contains("Queue is empty"));
    }

    #[test]
    fn queue_lists_items_verbatim() {
        let mut m = Model::new();
        to_watching(
            &mut m,
            vec![summary(
                "a1",
                "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
                "100000000000000000",
                false,
            )],
        );
        let rows = draw_rows(&m, 90, 20);
        // Address AND decimal wei on the SAME line, verbatim — a swap would split
        // them across lines.
        assert!(
            has_line_with(
                &rows,
                &[
                    "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
                    "100000000000000000",
                ],
            ),
            "address and amount must render together, exactly as received"
        );
    }

    #[test]
    fn open_card_renders_decoded_fields_and_raw_data_verbatim() {
        let mut m = Model::new();
        to_watching(&mut m, vec![summary("a1", "0xabc", "0", true)]);
        m.update(Msg::Open);
        let boxed = Box::new(Card {
            id: "a1".to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: Some(DecodedCall {
                method: "approve".to_owned(),
                spender: Some("0xdeadbeef".to_owned()),
                operator: None,
                from: None,
                to: None,
                token: None,
                amount: Some("0xffffffffffffffff".to_owned()),
                deadline: None,
                approved: None,
                is_unlimited: Some(true),
            }),
            high_risk: true,
            high_risk_reasons: vec!["unlimited_approval".to_owned()],
            raw_data: "0x095ea7b3deadbeef".to_owned(),
            not_after_unix: 1,
        });
        m.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(
            boxed,
        ))));
        let rows = draw_rows(&m, 100, 24);
        // Each value under its OWN label on one line — catches a field swap (e.g.
        // spender rendered where `to` should be).
        assert!(
            has_line_with(&rows, &["method", "approve"]),
            "method under its label"
        );
        assert!(
            has_line_with(&rows, &["spender", "0xdeadbeef"]),
            "spender under its label (not swapped into another field)"
        );
        // the 0x-hex amount is shown as received, not converted to a number
        assert!(
            has_line_with(&rows, &["amount", "0xffffffffffffffff"]),
            "hex amount verbatim under its label"
        );
        assert!(
            has_line_with(&rows, &["raw_data", "0x095ea7b3deadbeef"]),
            "raw_data verbatim under its label"
        );
        assert!(rows.iter().any(|r| r.contains("UNLIMITED")));
    }

    #[test]
    fn a_vanished_selection_note_is_shown() {
        let mut m = Model::new();
        to_watching(&mut m, vec![summary("a1", "0xabc", "0", false)]);
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(
            crate::protocol::GetOutcome::UnknownId,
        )));
        let screen = draw(&m, 90, 20);
        assert!(screen.contains("no longer available"));
    }

    #[test]
    fn fatal_phase_renders_the_reason() {
        let mut m = Model::new();
        m.update(Msg::Reply(Reply::Fatal(
            crate::transport::TransportError::NotConnected,
        )));
        let screen = draw(&m, 80, 10);
        assert!(screen.contains("wallet not running"));
    }

    fn card(id: &str, not_after_unix: u64, high_risk: bool) -> Box<Card> {
        Box::new(Card {
            id: id.to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: None,
            high_risk,
            high_risk_reasons: if high_risk {
                vec!["unlimited_approval".to_owned()]
            } else {
                vec![]
            },
            raw_data: "0x".to_owned(),
            not_after_unix,
        })
    }

    /// Drive the model to an open confirmation on a single queued item.
    fn open_card(model: &mut Model, id: &str, not_after_unix: u64, high_risk: bool) {
        to_watching(model, vec![summary(id, "0xabc", "0", high_risk)]);
        model.update(Msg::Open);
        model.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(
            card(id, not_after_unix, high_risk),
        ))));
    }

    #[test]
    fn a_native_send_card_leads_with_a_human_amount() {
        let mut m = Model::new();
        to_watching(
            &mut m,
            vec![summary("a1", "0xabc", "10000000000000000", false)],
        );
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(
            Box::new(Card {
                id: "a1".to_owned(),
                chain_id: 1,
                to: "0xabc".to_owned(),
                amount_wei: "10000000000000000".to_owned(),
                decoded_call: None,
                high_risk: false,
                high_risk_reasons: vec![],
                raw_data: "0x".to_owned(),
                not_after_unix: NOW + 27,
            }),
        ))));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["amount", "0.01 ETH"]),
            "the card leads with a human amount, not raw wei"
        );
    }

    #[test]
    fn a_plain_send_drops_the_decoded_call_noise() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false); // send, decoded_call: None
        let screen = draw(&m, 100, 24);
        assert!(
            !screen.contains("decoded_call: (none)"),
            "a plain send has nothing to decode — the noise line is gone"
        );
        assert!(
            has_line_with(&draw_rows(&m, 100, 24), &["to", "0xabc"]),
            "the recipient is still shown, in full"
        );
    }

    #[test]
    fn an_unselected_high_risk_queue_row_is_amber() {
        let mut m = Model::new();
        // Two items: the low-risk one is selected (index 0), the high-risk one is
        // not — so its amber is its own, not the selection highlight.
        to_watching(
            &mut m,
            vec![
                summary("a1", "0xabc", "0", false),
                summary("a2", "0xdef", "0", true),
            ],
        );
        let fgs = row_fgs_containing(&m, 80, 24, "0xdef");
        assert!(
            fgs.contains(&theme::high_risk()),
            "danger must read as amber before the card is even opened"
        );
    }

    #[test]
    fn the_decision_row_colors_approve_and_reject() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false); // low-risk send, approve armed
        let fgs = row_fgs_containing(&m, 80, 24, "Approve");
        assert!(fgs.contains(&theme::approve()), "Approve is teal");
        assert!(fgs.contains(&theme::reject()), "Reject is red");
    }

    #[test]
    fn the_countdown_rides_the_reject_button_never_the_approve_one() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);

        let row = action_row(&draw_rows(&m, 100, 24));
        // The Approve button closes at the first `]`; everything after it is Reject.
        let (approve_side, reject_side) = row.split_once(']').expect("two buttons render");

        assert!(approve_side.contains("Approve"));
        assert!(
            !approve_side.contains("27s"),
            "the deadline must never count down on the button that moves money \
             (AGENTS.md #5); found: {row}"
        );
        assert!(
            reject_side.contains("Reject") && reject_side.contains("auto in 27s"),
            "the countdown belongs to Reject, and says it will fire on its own: {row}"
        );
    }

    #[test]
    fn the_countdown_floors_at_zero_once_the_deadline_has_passed() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);

        let row = action_row(&draw_rows_at(&m, 100, 24, NOW + 99));
        assert!(
            row.contains("auto in 0s"),
            "an elapsed deadline reads as 0s, never as a wrapped-around eternity: {row}"
        );

        // An unreadable clock reaches the renderer as u64::MAX (`main::now_unix`).
        // It must floor to 0s too — a broken clock never buys the approval more time.
        let row = action_row(&draw_rows_at(&m, 100, 24, u64::MAX));
        assert!(
            row.contains("auto in 0s"),
            "an unreadable clock fails closed, it does not grant time: {row}"
        );
    }

    #[test]
    fn the_pin_prompt_moves_approve_onto_enter_and_masks_the_digits() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, true);
        m.update(Msg::Approve); // high risk: `y` opens the PIN prompt, it does not approve
        m.update(Msg::PinDigit('7'));
        m.update(Msg::PinDigit('3'));

        let rows = draw_rows(&m, 100, 24);
        let row = action_row(&rows);
        let screen = rows.join("\n");

        assert!(
            row.contains("enter  Approve") && row.contains("esc  Reject"),
            "while the PIN prompt is up, Enter approves and Esc rejects: {row}"
        );
        assert!(screen.contains("●●"), "two dots for two digits");
        assert!(!screen.contains("73"), "the digits must never render");
    }

    #[test]
    fn a_decision_on_the_wire_replaces_the_buttons() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);
        m.update(Msg::Reject);

        let screen = draw(&m, 100, 24);

        assert!(screen.contains("Sending your decision"));
        assert!(
            !screen.contains("Approve"),
            "with a decision on the wire there is no button left to press twice"
        );
    }

    /// The Stage-1 repro card: high-risk unlimited `approve` whose calldata used
    /// to push every warning off an 80×24 screen.
    fn risk_card(id: &str, raw_data: String) -> Box<Card> {
        Box::new(Card {
            id: id.to_owned(),
            chain_id: 1,
            to: "0x742d35Cc6634C0532925a3b844Bc454e4438f44e".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: Some(DecodedCall {
                method: "approve".to_owned(),
                spender: Some("0xdeadbeef".to_owned()),
                operator: None,
                from: None,
                to: None,
                token: None,
                amount: Some("0xffffffffffffffff".to_owned()),
                deadline: None,
                approved: None,
                is_unlimited: Some(true),
            }),
            high_risk: true,
            high_risk_reasons: vec!["unlimited_approval".to_owned()],
            raw_data,
            not_after_unix: NOW + 27,
        })
    }

    /// Drive the model to an open confirmation on `risk_card`.
    fn open_risk_card(model: &mut Model, raw_data: String) {
        to_watching(model, vec![summary("a1", "0xabc", "0", true)]);
        model.update(Msg::Open);
        model.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(
            risk_card("a1", raw_data),
        ))));
    }

    /// The first row index containing `needle` — for order checks.
    fn row_of(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    /// 328-char calldata, as measured in the Stage-1 repro.
    fn stage1_raw_data() -> String {
        let raw = format!("0x{}", "ab".repeat(163));
        assert_eq!(raw.chars().count(), 328);
        raw
    }

    #[test]
    fn chunking_respects_display_width_and_never_splits_a_char() {
        // Exact multiples: no empty trailing chunk.
        assert_eq!(chunk_display_width("abcdef", 3), vec!["abc", "def"]);
        // A 2-cell char that does not fit the remaining cell starts a new chunk.
        assert_eq!(chunk_display_width("ab漢", 3), vec!["ab", "漢"]);
        // Empty input still claims one (blank) row.
        assert_eq!(chunk_display_width("", 5), vec![""]);
    }

    #[test]
    fn every_warning_and_the_pin_prompt_stay_on_screen_with_a_long_raw_data() {
        let mut m = Model::new();
        open_risk_card(&mut m, stage1_raw_data());
        m.update(Msg::Approve); // high risk: opens the PIN prompt
        m.update(Msg::PinDigit('7'));

        let rows = draw_rows(&m, 80, 24);
        let screen = rows.join("\n");

        assert!(
            screen.contains("HIGH RISK"),
            "the risk warning must never leave the screen"
        );
        assert!(
            screen.contains("UNLIMITED"),
            "the unlimited-amount warning must never leave the screen"
        );
        assert!(
            screen.contains("enter your PIN"),
            "the PIN prompt must never leave the screen"
        );
        assert!(
            screen.contains("●"),
            "the PIN dots must be visible — a blind PIN entry is not an entry"
        );
        // raw_data is the one elastic element, so it renders BELOW every warning.
        assert!(
            row_of(&rows, "HIGH RISK") < row_of(&rows, "raw_data"),
            "raw_data must render below the risk warning, never above it"
        );
    }

    #[test]
    fn high_risk_and_unlimited_stay_on_screen_with_a_long_raw_data_without_pin() {
        let mut m = Model::new();
        open_risk_card(&mut m, stage1_raw_data());

        let screen = draw(&m, 80, 24);

        assert!(screen.contains("HIGH RISK"));
        assert!(screen.contains("UNLIMITED"));
    }

    #[test]
    fn a_short_raw_data_still_renders_whole_with_no_truncation_marker() {
        let mut m = Model::new();
        open_risk_card(&mut m, "0x095ea7b3deadbeef".to_owned());

        let rows = draw_rows(&m, 80, 24);

        assert!(
            has_line_with(&rows, &["raw_data: 0x095ea7b3deadbeef"]),
            "a raw_data that fits renders whole, exactly as received"
        );
        assert!(
            !rows.join("\n").contains("not shown"),
            "no truncation marker when nothing was truncated"
        );
    }

    #[test]
    fn an_overlong_raw_data_is_truncated_with_an_explicit_marker_not_silently() {
        let mut m = Model::new();
        open_risk_card(&mut m, format!("0x{}", "ab".repeat(1000)));

        let rows = draw_rows(&m, 80, 24);
        let screen = rows.join("\n");

        assert!(
            has_line_with(&rows, &["raw_data: 0xabab"]),
            "the head of raw_data is still shown"
        );
        assert!(
            screen.contains("not shown"),
            "a clipped raw_data must say so out loud, never trail off silently"
        );

        // The marker's numbers are the honesty of this screen: they must name
        // the real payload and account for every char of it.
        let marker_row = rows
            .iter()
            .find(|r| r.contains("not shown"))
            .expect("the truncation marker renders");
        let nums: Vec<usize> = marker_row
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(
            nums.len(),
            3,
            "total, shown and not-shown counters: {marker_row}"
        );
        assert_eq!(nums[0], 2002, "the total names the real payload size");
        assert_eq!(
            nums[1] + nums[2],
            nums[0],
            "shown + not shown must account for every char: {marker_row}"
        );
        assert!(
            nums[1] < nums[2],
            "a 2002-char payload on 24 rows is mostly hidden — the shown and \
             not-shown counters look swapped: {marker_row}"
        );
    }

    #[test]
    fn a_cramped_terminal_pulls_approve_and_says_why() {
        let mut m = Model::new();
        open_risk_card(&mut m, stage1_raw_data());

        // 80×13: the stage-1 card's priority fields cannot all fit (B4). The card
        // is one row shorter since v2 (a zero native value is no longer headlined),
        // so the too-small boundary moved down by one row.
        let rows = draw_rows(&m, 80, 13);
        let screen = rows.join("\n");

        assert!(
            screen.contains("TOO SMALL"),
            "the human is told the card is cut, never left guessing"
        );
        assert!(
            !screen.contains("Approve"),
            "no live Approve button on a card the human cannot read"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("Reject") && r.contains("auto in")),
            "reject and its countdown survive at any size (AGENTS.md #5)"
        );
    }

    #[test]
    fn the_gate_lifts_when_the_terminal_grows() {
        let mut m = Model::new();
        open_risk_card(&mut m, stage1_raw_data());

        let screen = draw(&m, 80, 24);

        assert!(screen.contains("Approve"), "a full card arms the button");
        assert!(!screen.contains("TOO SMALL"), "no banner on a full card");
    }

    #[test]
    fn the_outcome_notice_names_the_decision_and_shows_the_tx_hash() {
        // Resident: the decision renders as a notice on the still-living
        // queue screen, not as a terminal screen.
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(
            crate::protocol::ResolveOutcome::Executed {
                tx_hash: "0xfeed".to_owned(),
            },
        )));

        let screen = draw(&m, 80, 24);

        assert!(screen.contains("APPROVED"));
        assert!(screen.contains("0xfeed"), "the tx hash is shown verbatim");
        assert!(
            screen.contains("Queue·"),
            "the queue screen (tab bar) is still alive behind the notice"
        );
    }

    #[test]
    fn the_lockout_notice_counts_down_and_names_the_fail_closed_denies() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, true);
        m.update(Msg::Approve); // opens the PIN prompt
        m.update(Msg::PinDigit('1'));
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Resolve(
            crate::protocol::ResolveOutcome::Locked { retry_after_s: 300 },
        )));

        let screen = draw(&m, 80, 24);

        assert!(screen.contains("PIN locked"));
        assert!(
            screen.contains("pending items were denied"),
            "only PENDING items are denied by the fail-closed drop (§4) — an \
             executing item is untouched, and the text must not bury it"
        );
        assert!(screen.contains("~300s"));
    }

    #[test]
    fn the_card_shows_a_two_block_from_to_flow_with_full_addresses() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);

        let rows = draw_rows(&m, 80, 24);

        assert!(
            has_line_with(&rows, &["from", "your wallet"]),
            "the From block names the wallet"
        );
        assert!(
            has_line_with(&rows, &[WALLET]),
            "the wallet address renders in FULL, verbatim — address-poisoning \
             hides in shortened addresses"
        );
        assert!(has_line_with(&rows, &["to", "0xabc"]));
    }

    // ── nav-shell: tab bar + the Receive view ──

    /// Background colors on the first rendered row containing `needle` —
    /// the QR's white ground is as load-bearing as its black ink.
    fn row_bgs_containing(
        model: &Model,
        w: u16,
        h: u16,
        needle: &str,
    ) -> Vec<ratatui::style::Color> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, model, NOW)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..h {
            let text: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
            if text.contains(needle) {
                return (0..w).filter_map(|x| buffer[(x, y)].style().bg).collect();
            }
        }
        Vec::new()
    }

    /// Rows that carry QR half-blocks.
    fn qr_rows(rows: &[String]) -> usize {
        rows.iter()
            .filter(|r| r.contains('█') || r.contains('▀') || r.contains('▄'))
            .count()
    }

    #[test]
    fn the_tab_bar_names_both_views_with_their_keys_and_the_pending_count() {
        let mut m = Model::new();
        to_watching(&mut m, vec![summary("a1", "0xabc", "0", false)]);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["Queue·1 [a]", "Receive [r]"]),
            "both tabs, their keys, and the live pending count on one line"
        );
    }

    #[test]
    fn the_active_tab_is_highlighted() {
        use ratatui::style::Modifier;
        let mut m = Model::new();
        to_watching(&mut m, vec![]);

        // On the queue view, the Queue tab is the reversed one.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &m, NOW)).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..80).map(|x| buffer[(x, 0)].symbol()).collect();
        let queue_at = row.find("Queue").expect("the Queue tab renders") as u16;
        let receive_at = row.find("Receive").expect("the Receive tab renders") as u16;
        assert!(
            buffer[(queue_at, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "the active tab reads as selected"
        );
        assert!(
            !buffer[(receive_at, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "the inactive tab does not"
        );
        // COLOR too, not just the modifier: swapping accent() for the muted
        // label color would keep REVERSED and pass a modifier-only check.
        assert_eq!(
            buffer[(queue_at, 0)].style().fg,
            Some(theme::accent()),
            "the active tab carries the brand accent"
        );
    }

    #[test]
    fn the_receive_view_shows_the_full_address_and_a_scannable_qr() {
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &[WALLET]),
            "the wallet address renders in FULL on one row, verbatim EIP-55"
        );
        // Of the 19 QR text rows, the top and bottom 2 are pure quiet zone
        // (spaces) — exactly 15 carry ink. Fewer would mean a clipped code.
        assert_eq!(
            qr_rows(&rows),
            15,
            "the version-3 QR renders whole: every ink row present"
        );
        assert!(
            !rows.join("\n").contains("QR hidden"),
            "a fitting QR shows no marker"
        );
        // COLOR carries the scan contrast: black ink on white ground,
        // regardless of the terminal theme (canon: assert the style, not
        // just the text).
        let fgs = row_fgs_containing(&m, 80, 24, "█");
        assert!(
            fgs.contains(&ratatui::style::Color::Rgb(0, 0, 0)),
            "QR ink is true black"
        );
        let bgs = row_bgs_containing(&m, 80, 24, "█");
        assert!(
            bgs.contains(&ratatui::style::Color::Rgb(0xFF, 0xFF, 0xFF)),
            "QR ground is true white"
        );
    }

    #[test]
    fn a_short_terminal_hides_the_qr_with_an_explicit_marker() {
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 80, 12);
        assert!(
            has_line_with(&rows, &[WALLET]),
            "the address — the priority element — still renders in full"
        );
        assert!(
            rows.join("\n").contains("QR hidden"),
            "the missing QR says so out loud"
        );
        assert_eq!(qr_rows(&rows), 0, "no partial QR ever renders");
    }

    #[test]
    fn a_narrow_terminal_hides_the_qr_rather_than_wrap_it() {
        // 30 columns: the 37-column QR would fold under Wrap into something
        // that still LOOKS scannable — and scans as garbage (/check-2).
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 30, 24);
        assert!(rows.join("\n").contains("QR hidden"));
        assert_eq!(qr_rows(&rows), 0, "never a wrapped QR");
        assert!(
            has_line_with(&rows, &["your address"]),
            "the address block is still there (wrapped by cells, not clipped)"
        );
    }

    #[test]
    fn a_degraded_context_shows_no_receive_address_and_no_qr() {
        let mut m = Model::new();
        to_watching_no_context(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 80, 24);
        assert!(
            rows.join("\n").contains("no receive address"),
            "an honest degradation, not a fabricated code"
        );
        assert_eq!(qr_rows(&rows), 0, "a QR of nothing must never render");
        assert!(
            !has_line_with(&rows, &["your address"]),
            "no address block without an address"
        );
    }

    #[test]
    fn the_qr_fit_gate_sits_exactly_on_its_boundaries() {
        // T1: every existing snapshot sits far from the `<=` thresholds — an
        // off-by-one in either comparison would slip through. Pin both
        // boundaries with an equality case and its neighbour.
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        // Height boundary at width 80: the address fits one row, so the
        // inner column is label(1) + address(1) + QR(19) = 21 = inner height
        // of a 24-row terminal (1 tab + 2 borders). 24 is the equality case.
        assert_eq!(qr_rows(&draw_rows(&m, 80, 24)), 15, "equality fits");
        assert_eq!(qr_rows(&draw_rows(&m, 80, 23)), 0, "one row short hides");
        assert!(draw_rows(&m, 80, 23).join("\n").contains("QR hidden"));

        // Width boundary: the QR is 37 columns; borders make the terminal
        // 39. At 39 the inner width equals 37 exactly (the address wraps to
        // two rows, still leaving 22 ≤ 27 inner rows at height 30).
        assert_eq!(qr_rows(&draw_rows(&m, 39, 30)), 15, "equality fits");
        assert_eq!(qr_rows(&draw_rows(&m, 38, 30)), 0, "one col short hides");
        assert!(draw_rows(&m, 38, 30).join("\n").contains("QR hidden"));
    }

    #[test]
    fn an_empty_address_string_degrades_like_a_missing_context() {
        // T2: `parse_context` rejects a MISSING address but passes "" — the
        // view must not label an empty line "your address" nor fabricate a
        // scannable QR of nothing (/check-4).
        let mut m = Model::new();
        to_watching_empty_address(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 80, 24);
        assert!(
            rows.join("\n").contains("no receive address"),
            "an empty address is no address"
        );
        assert_eq!(qr_rows(&rows), 0);
        assert!(!has_line_with(&rows, &["your address"]));
    }

    #[test]
    fn a_tiny_terminal_says_the_address_is_cut_never_cuts_it_in_silence() {
        // МИНОР-1 (Гейт-2): when even label + address + marker overflow the
        // panel, the top row must say so — a silently clipped address is a
        // half-address someone may copy.
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.update(Msg::View(crate::app::View::Receive));

        let rows = draw_rows(&m, 80, 5); // inner height 2 < the 3 lines built
        assert!(
            rows.join("\n").contains("TOO SMALL"),
            "the overflow banner takes the guaranteed-visible row"
        );
    }

    #[test]
    fn the_notice_is_queue_furniture_and_does_not_render_on_receive() {
        // /check-5: the slot survives the switch untouched — back on the
        // queue, the human still sees what happened.
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(
            crate::protocol::ResolveOutcome::Executed {
                tx_hash: "0xfeed".to_owned(),
            },
        )));
        m.update(Msg::View(crate::app::View::Receive));
        assert!(
            !draw(&m, 80, 24).contains("APPROVED"),
            "the outcome notice stays off the Receive screen"
        );
        m.update(Msg::View(crate::app::View::Queue));
        assert!(
            draw(&m, 80, 24).contains("APPROVED"),
            "and is still there when the human returns"
        );
    }

    #[test]
    fn a_degraded_context_falls_back_to_the_to_only_card() {
        let mut m = Model::new();
        to_watching_no_context(&mut m, vec![summary("a1", "0xabc", "5", false)]);
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(
            Box::new(Card {
                id: "a1".to_owned(),
                chain_id: 1,
                to: "0xabc".to_owned(),
                amount_wei: "5".to_owned(),
                decoded_call: None,
                high_risk: false,
                high_risk_reasons: vec![],
                raw_data: "0x".to_owned(),
                not_after_unix: NOW + 27,
            }),
        ))));

        let rows = draw_rows(&m, 80, 24);

        assert!(
            !has_line_with(&rows, &["your wallet"]),
            "no From block without the wallet context"
        );
        assert!(has_line_with(&rows, &["to", "0xabc"]));
        // The display degraded — approve must NOT be gated on it.
        let row = action_row(&rows);
        assert!(row.contains("Approve"));
    }
}
