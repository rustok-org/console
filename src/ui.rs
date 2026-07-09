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
    // screen that must never be silently clipped (`AGENTS.md` #1).
    let mut constraints = vec![
        Constraint::Length(1), // header
        Constraint::Min(3),    // queue
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

/// Render the open confirmation's card **verbatim** — the core's fields as
/// received, no re-derivation. `None` shows a hint to open one.
fn render_detail(frame: &mut Frame, confirm: Option<&Confirm>, area: ratatui::layout::Rect) {
    let block = Block::bordered().title(" Card ");
    let Some(confirm) = confirm else {
        let hint =
            Paragraph::new("Select a request and press enter to see the full card.").block(block);
        frame.render_widget(hint, area);
        return;
    };
    let card: &Card = confirm.card();

    let mut lines = vec![
        kv("to", &card.to),
        kv("amount_wei", &card.amount_wei),
        kv("chain_id", &card.chain_id.to_string()),
        kv("raw_data", &card.raw_data),
    ];
    if card.high_risk {
        lines.push(Line::from(Span::styled(
            format!("HIGH RISK: {}", card.high_risk_reasons.join(", ")),
            Style::new().add_modifier(Modifier::BOLD),
        )));
    }
    match &card.decoded_call {
        None => lines.push(Line::from("decoded_call: (none)")),
        Some(dc) => {
            lines.push(Line::from(format!("decoded_call.method: {}", dc.method)));
            push_opt(&mut lines, "spender", dc.spender.as_deref());
            push_opt(&mut lines, "operator", dc.operator.as_deref());
            push_opt(&mut lines, "from", dc.from.as_deref());
            push_opt(&mut lines, "to", dc.to.as_deref());
            push_opt(&mut lines, "token", dc.token.as_deref());
            push_opt(&mut lines, "amount", dc.amount.as_deref());
            push_opt(&mut lines, "deadline", dc.deadline.as_deref());
            if dc.is_unlimited == Some(true) {
                lines.push(Line::from(Span::styled(
                    "amount: UNLIMITED",
                    Style::new().add_modifier(Modifier::BOLD),
                )));
            }
        }
    }
    if let Some(pin_len) = confirm.pin_len() {
        lines.push(Line::from(""));
        lines.push(Line::from("High-risk approval — enter your PIN:"));
        // Only the count is shown — never the digits.
        lines.push(Line::from(Span::styled(
            "●".repeat(pin_len),
            Style::new().add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(err) = confirm.error() {
        lines.push(Line::from(""));
        lines.push(Line::from(resolve_error_text(err)));
    }

    // Wrap (never truncate): a clear-signing card must show the whole value —
    // a silently clipped raw_data or address would be the one lie this screen
    // exists to prevent. trim: false keeps the exact bytes, including leading space.
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
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

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(format!("{key}: {value}"))
}

fn push_opt(lines: &mut Vec<Line<'static>>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        lines.push(Line::from(format!("decoded_call.{key}: {v}")));
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
