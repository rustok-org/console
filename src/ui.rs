//! Rendering — a pure function of the [`Model`], immediate-mode (rebuilt every
//! frame). The console shows the core's values **verbatim** (`AGENTS.md` #1): the
//! card renders the fields as received, adding no interpretation. The PIN is shown
//! only as a row of dots — never the digits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::app::{AuthError, Model, Phase};
use crate::protocol::{Card, Summary};

/// Render the whole screen for the current model.
pub fn render(frame: &mut Frame, model: &Model) {
    match model.phase() {
        Phase::Connecting => {
            render_centered(frame, "Connecting to the wallet…");
        }
        Phase::Authing { pin, error } => render_auth(frame, pin.len(), error.as_ref()),
        Phase::Watching {
            items,
            selected,
            card,
            note,
        } => render_watch(frame, items, *selected, card.as_deref(), note.as_deref()),
        Phase::Fatal(err) => render_centered(frame, &err.to_string()),
    }
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
    card: Option<&Card>,
    note: Option<&str>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(3),    // queue
        Constraint::Min(6),    // card / hint
        Constraint::Length(1), // note / footer
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(format!(" Pending approvals: {}", items.len())),
        chunks[0],
    );

    render_queue(frame, items, selected, chunks[1]);
    render_detail(frame, card, chunks[2]);

    let footer = note.unwrap_or("↑/↓ select · enter open · q quit  (approve/deny in C-PR-1b)");
    frame.render_widget(Paragraph::new(footer), chunks[3]);
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

/// Render the selected card **verbatim** — the core's fields as received, no
/// re-derivation. `None` shows a hint to open one.
fn render_detail(frame: &mut Frame, card: Option<&Card>, area: ratatui::layout::Rect) {
    let block = Block::bordered().title(" Card ");
    let Some(card) = card else {
        let hint =
            Paragraph::new("Select a request and press enter to see the full card.").block(block);
        frame.render_widget(hint, area);
        return;
    };

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
    frame.render_widget(Paragraph::new(lines).block(block), area);
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

    /// Render the model into a fixed grid and flatten the buffer to a String for
    /// substring assertions.
    fn draw(model: &Model, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, model)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
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
        let screen = draw(&m, 90, 20);
        // the address and the decimal wei render exactly as received (verbatim).
        assert!(screen.contains("0x742d35Cc6634C0532925a3b844Bc454e4438f44e"));
        assert!(screen.contains("100000000000000000"));
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
        let screen = draw(&m, 100, 24);
        assert!(screen.contains("approve"), "method rendered");
        assert!(screen.contains("0xdeadbeef"), "spender verbatim");
        // the 0x-hex amount is shown as received, not converted to a number
        assert!(screen.contains("0xffffffffffffffff"), "hex amount verbatim");
        assert!(screen.contains("0x095ea7b3deadbeef"), "raw_data verbatim");
        assert!(screen.contains("UNLIMITED"));
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
}
