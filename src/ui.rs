//! Rendering — a pure function of the [`Model`], immediate-mode (rebuilt every
//! frame). The console shows the core's values **verbatim** (`AGENTS.md` #1): the
//! card renders the fields as received, adding no interpretation. The PIN is shown
//! only as a row of dots — never the digits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{AuthError, Confirm, ExitOutcome, Model, Phase, ResolveError};
use crate::protocol::{Card, ResolveOutcome, Summary};

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
            note,
        } => render_watch(
            frame,
            items,
            *selected,
            confirm.as_deref(),
            note.as_deref(),
            now_unix,
        ),
        Phase::Resolved { outcome, exit } => render_centered(frame, &resolved_text(outcome, *exit)),
        Phase::Fatal(err) => render_centered(frame, &err.to_string()),
    }
}

/// The terminal answer, shown verbatim before the console exits.
fn resolved_text(outcome: &ResolveOutcome, exit: ExitOutcome) -> String {
    let detail = match outcome {
        ResolveOutcome::Executed { tx_hash } => format!("executed — {tx_hash}"),
        ResolveOutcome::Failed { reason } => format!("execution failed — {reason}"),
        ResolveOutcome::Denied => "denied".to_owned(),
        ResolveOutcome::AlreadyResolved { state } => {
            format!("already resolved by someone else ({state:?})")
        }
        other => format!("{other:?}"),
    };
    let headline = match exit {
        ExitOutcome::Approved => "APPROVED",
        ExitOutcome::Rejected => "REJECTED",
        ExitOutcome::Expired => "EXPIRED",
        ExitOutcome::Failed => "FAILED",
    };
    format!("{headline}\n\n{detail}\n\nPress any key to close.")
}

fn render_centered(frame: &mut Frame, message: &str) {
    let block = Block::bordered().title(" Rustok Console ");
    let paragraph = Paragraph::new(message).block(block);
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
        lines.push(Line::from(auth_error_text(err)));
    }
    let paragraph = Paragraph::new(lines).block(Block::bordered().title(" Unlock "));
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

fn render_watch(
    frame: &mut Frame,
    items: &[Summary],
    selected: usize,
    confirm: Option<&Confirm>,
    note: Option<&str>,
    now_unix: u64,
) {
    // The note's row is claimed only when there is a note. An always-reserved row
    // would take its space from the card, and the card is the one thing on this
    // screen whose priority fields must never leave the screen (`AGENTS.md` #1).
    //
    // While a confirmation is open the card is the decision surface: the queue
    // collapses to a single-item strip (the List keeps the selection in view)
    // and the card takes every remaining row. Splitting the height evenly would
    // starve the card of the rows its risk warnings and PIN prompt need on a
    // 24-row terminal.
    let queue_rows = if confirm.is_some() {
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
    if note.is_some() {
        constraints.push(Constraint::Length(1)); // transient note
    }
    let chunks = Layout::vertical(constraints).split(frame.area());

    frame.render_widget(
        Paragraph::new(format!(" Pending approvals: {}", items.len())),
        chunks[0],
    );

    render_queue(frame, items, selected, chunks[1]);
    render_detail(frame, confirm, chunks[2]);
    render_actions(frame, confirm, now_unix, chunks[3]);

    if let Some(note) = note {
        frame.render_widget(Paragraph::new(note), chunks[4]);
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
    now_unix: u64,
    area: ratatui::layout::Rect,
) {
    let Some(confirm) = confirm else {
        frame.render_widget(Paragraph::new("  ↑/↓ select · enter open · q quit"), area);
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

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::raw(format!("[ {approve_key}  Approve ]")),
            Span::raw("    "),
            Span::styled(
                format!("[ {reject_key}  Reject · auto in {left}s ]"),
                Style::new()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED),
            ),
        ])),
        area,
    );
}

fn render_queue(
    frame: &mut Frame,
    items: &[Summary],
    selected: usize,
    area: ratatui::layout::Rect,
) {
    let block = Block::bordered().title(" Queue ");
    if items.is_empty() {
        let empty = Paragraph::new("Queue is empty — waiting for approval requests…").block(block);
        frame.render_widget(empty, area);
        return;
    }
    let rows: Vec<ListItem> = items
        .iter()
        .map(|s| {
            let flag = if s.high_risk { "⚠ " } else { "  " };
            ListItem::new(format!(
                "{flag}{kind:5} {to}  {amount} wei",
                kind = kind_word(s),
                to = s.to,
                amount = s.amount_wei
            ))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(selected.min(items.len().saturating_sub(1))));
    let list = List::new(rows)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

fn kind_word(s: &Summary) -> &'static str {
    match s.kind {
        crate::protocol::Kind::Send => "send",
        crate::protocol::Kind::Call => "call",
    }
}

/// Render the open confirmation's card — the core's fields **verbatim**, no
/// re-derivation. `None` shows a hint to open one.
///
/// Priority fields (everything except `raw_data`) render first, and
/// `raw_data` — the only elastic element — gets exactly the rows that remain,
/// truncated with an explicit marker when it cannot fit. A long calldata can
/// therefore never push a risk warning or the PIN prompt off the screen. That
/// guarantee is against `raw_data`: priority fields that alone overflow the
/// card (pathological server data) still clip at the bottom until scrolling
/// lands. Every line is pre-wrapped to the card's inner width so one logical
/// line is one visual row and the height budget is exact, not an estimate.
fn render_detail(frame: &mut Frame, confirm: Option<&Confirm>, area: ratatui::layout::Rect) {
    let block = Block::bordered().title(" Card ");
    let Some(confirm) = confirm else {
        let hint =
            Paragraph::new("Select a request and press enter to see the full card.").block(block);
        frame.render_widget(hint, area);
        return;
    };
    let card: &Card = confirm.card();

    let inner = block.inner(area);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let bold = Style::new().add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();
    push_wrapped(&mut lines, width, format!("to: {}", card.to), Style::new());
    push_wrapped(
        &mut lines,
        width,
        format!("amount_wei: {}", card.amount_wei),
        Style::new(),
    );
    push_wrapped(
        &mut lines,
        width,
        format!("chain_id: {}", card.chain_id),
        Style::new(),
    );
    if card.high_risk {
        push_wrapped(
            &mut lines,
            width,
            format!("HIGH RISK: {}", card.high_risk_reasons.join(", ")),
            bold,
        );
    }
    match &card.decoded_call {
        None => lines.push(Line::from("decoded_call: (none)")),
        Some(dc) => {
            push_wrapped(
                &mut lines,
                width,
                format!("decoded_call.method: {}", dc.method),
                Style::new(),
            );
            push_opt(&mut lines, width, "spender", dc.spender.as_deref());
            push_opt(&mut lines, width, "operator", dc.operator.as_deref());
            push_opt(&mut lines, width, "from", dc.from.as_deref());
            push_opt(&mut lines, width, "to", dc.to.as_deref());
            push_opt(&mut lines, width, "token", dc.token.as_deref());
            push_opt(&mut lines, width, "amount", dc.amount.as_deref());
            push_opt(&mut lines, width, "deadline", dc.deadline.as_deref());
            if dc.is_unlimited == Some(true) {
                push_wrapped(&mut lines, width, "amount: UNLIMITED".to_owned(), bold);
            }
        }
    }
    if let Some(pin_len) = confirm.pin_len() {
        lines.push(Line::from(""));
        push_wrapped(
            &mut lines,
            width,
            "High-risk approval — enter your PIN:".to_owned(),
            Style::new(),
        );
        // Only the count is shown — never the digits.
        push_wrapped(&mut lines, width, "●".repeat(pin_len), bold);
    }
    if let Some(err) = confirm.error() {
        lines.push(Line::from(""));
        push_wrapped(&mut lines, width, resolve_error_text(err), Style::new());
    }

    // raw_data comes LAST and absorbs whatever rows the priority fields above
    // left over. Never truncate priority fields; a truncated raw_data says so
    // out loud — a silent clip would be the one lie this screen exists to
    // prevent. Wrap stays on as a backstop only (every line already fits the
    // width); trim: false keeps the exact bytes, including leading space.
    let budget = height.saturating_sub(lines.len());
    push_raw_data(&mut lines, width, budget, &card.raw_data);

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
        ResolveError::Locked(secs) => format!("Locked out. Try again in {secs}s."),
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
    use crate::protocol::{AuthOutcome, Card, DecodedCall, Kind, Risk};
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

    fn to_watching(model: &mut Model, items: Vec<Summary>) {
        model.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        model.update(Msg::PinDigit('1'));
        model.update(Msg::PinSubmit);
        model.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
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
    fn the_resolved_screen_names_the_outcome_and_shows_the_tx_hash() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Executed {
            tx_hash: "0xfeed".to_owned(),
        })));

        let screen = draw(&m, 80, 12);

        assert!(screen.contains("APPROVED"));
        assert!(screen.contains("0xfeed"), "the tx hash is shown verbatim");
    }
}
