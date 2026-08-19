//! Rendering — a pure function of the [`Model`], immediate-mode (rebuilt every
//! frame). The console shows the core's values **verbatim** (`AGENTS.md` #1): the
//! card renders the fields as received, adding no interpretation. The PIN is shown
//! only as a row of dots — never the digits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph, Wrap};

use crate::app::{
    AuthError, Confirm, DecisionKind, HistoryEntry, ModeSwitch, Model, Notice, Phase, Positions,
    ResolveError, View,
};
use crate::protocol::{Card, Kind, OutcomeState, Policy, PolicyMode, PolicyOrigin, Summary};
use crate::{format, qr, theme};

/// Render the whole screen for the current model.
///
/// `now_unix` is the wall clock, passed in rather than read here: the [`Model`]
/// stays a pure function of its messages, and the countdown stays testable.
pub fn render(frame: &mut Frame, model: &Model, now_unix: u64, versions: Versions<'_>) {
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
            switch,
        } => match view {
            View::Queue => render_watch(
                frame,
                items,
                *selected,
                confirm.as_deref(),
                notice.as_ref(),
                WalletView {
                    address: model.wallet_address(),
                    policy: model.policy(),
                },
                now_unix,
            ),
            View::Receive => {
                render_receive(frame, items.len(), model.wallet_address(), model.policy())
            }
            View::Dashboard => {
                render_dashboard(frame, items.len(), model, versions);
                if let Some(s) = switch {
                    render_mode_switch(frame, s, model.policy().mode);
                }
            }
            View::Activity => render_activity(frame, items.len(), model, now_unix),
        },
        Phase::Fatal(err) => render_centered(frame, &err.to_string()),
    }
}

/// The wallet's autonomy as one phrase for the header, with the style that
/// carries its meaning — or `None` when there is nothing truthful to say yet.
///
/// **The alarm colour is spent on exactly one state.** `autonomous` +
/// `provisioned` is the only combination where what the human expects and what
/// the wallet does come apart: the mode reads "sends by itself" while every
/// send parks. Acknowledged autonomy is not an alarm — it is the human's own
/// choice, and colouring it like danger would spend the signal that makes the
/// one real case stand out.
///
/// `PolicyMode::Unknown` renders **nothing**. The header states a fact about
/// this wallet; before the first `context` reply lands there is no fact, and an
/// invented placeholder would be a claim we cannot back.
///
/// Two lengths, because the phrase shares one row with the tabs, and **both of
/// them are ratified strings** — the long one transcribed from the design
/// decision (§2), the short one approved with it. No third, in-between wording
/// is invented here: a paraphrase in the one place that carries the alarm
/// colour is exactly the kind of drift nobody notices.
///
/// The tabs take 59 columns and the phrase is 45, so the long form needs 105
/// to appear (measured, not estimated — the earlier "~115" was neither); a
/// standard 80-column terminal leaves 21 and gets the short one. The short form
/// keeps the half that changes what the human does, and the instruction itself
/// lives in the Dashboard banner — truncating the long form instead would cut
/// exactly that half off the end.
fn mode_phrase(policy: Policy, budget: usize) -> Option<Span<'static>> {
    let (full, short, style) = match (policy.mode, policy.origin) {
        (PolicyMode::Unknown, _) => return None,
        (PolicyMode::ReadOnly, _) => (
            "read-only",
            "read-only",
            Style::new().fg(theme::high_risk()),
        ),
        (PolicyMode::Supervised, _) => ("manual mode", "manual mode", theme::label_style()),
        (PolicyMode::Autonomous, PolicyOrigin::Acknowledged) => (
            "autonomous · confirmed",
            "confirmed",
            Style::new().fg(theme::accent()),
        ),
        (PolicyMode::Autonomous, PolicyOrigin::Provisioned) => (
            "autonomous · unconfirmed — sends wait for you",
            "unconfirmed",
            Style::new()
                .fg(theme::high_risk())
                .add_modifier(Modifier::BOLD),
        ),
    };
    let text = if full.chars().count() <= budget {
        full
    } else {
        short
    };
    Some(Span::styled(text, style))
}

/// What the chrome needs to know about the wallet itself: who it is, and how it
/// behaves. Travelling together because they are read together — the header
/// states the policy, the card's From block states the address, and both are
/// facts about this wallet rather than about the item being decided.
#[derive(Clone, Copy)]
struct WalletView<'a> {
    address: Option<&'a str>,
    policy: Policy,
}

/// The header row: tabs on the left, the wallet's autonomy on the right.
///
/// The phrase is on **every** screen, not only the one that can act on it —
/// Q5's rule is that the mode is loud at every use, not readable on request.
/// It is pushed right by padding rather than by a nested layout so the tab
/// bar's own geometry (and with it the card's, §`watch_chunks`) is untouched.
/// When the phrase cannot fit at all, the tabs win: navigation must not be
/// unreachable, and the state still has the Dashboard banner.
fn header_line(active: View, pending: usize, policy: Policy, width: u16) -> Line<'static> {
    let mut line = tab_line(active, pending);
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let width = usize::from(width);
    let Some(budget) = width.checked_sub(used + 1) else {
        return line;
    };
    let Some(phrase) = mode_phrase(policy, budget) else {
        return line;
    };
    let len = phrase.content.chars().count();
    if len > budget {
        return line;
    }
    line.spans.push(Span::raw(" ".repeat(width - used - len)));
    line.spans.push(phrase);
    line
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
        tab(" Dashboard [d] ".to_owned(), active == View::Dashboard),
        Span::raw(" "),
        tab(format!(" Queue·{pending} [a] "), active == View::Queue),
        Span::raw(" "),
        tab(" Receive [r] ".to_owned(), active == View::Receive),
        Span::raw(" "),
        tab(" Activity [h] ".to_owned(), active == View::Activity),
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
/// The screen has two shapes, and the card exists in only one of them.
///
/// **Confirmation open** — the card is the decision surface: the queue
/// collapses to a single-item strip (the List keeps the selection in view) and
/// the card takes every remaining row. Splitting the height evenly would starve
/// the card of the rows its risk warnings and PIN prompt need on a 24-row
/// terminal.
///
/// **Confirmation closed** — there is no card row at all, and the queue takes
/// the whole middle. The rows the card needs are needed only while it is open,
/// and by then the queue has already collapsed and freed them: a permanent
/// reserve was protecting a case in which the reserve is not required. The
/// "enter to open" hint moves to the navigation row, which is one row, not six.
fn watch_chunks(
    area: ratatui::layout::Rect,
    confirm_open: bool,
    has_note: bool,
) -> std::rc::Rc<[ratatui::layout::Rect]> {
    let mut constraints = vec![Constraint::Length(1)]; // header
    if confirm_open {
        constraints.push(Constraint::Length(3)); // queue: borders + selected row
        constraints.push(Constraint::Min(6)); // card
    } else {
        // No card, no reserve: the list takes the rows the card used to hold
        // empty, and the "enter to open" hint lives in the navigation row
        // below — one row is all it needs.
        constraints.push(Constraint::Min(3)); // queue
    }
    constraints.push(Constraint::Length(1)); // decision row / navigation hint
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
    wallet: WalletView<'_>,
    now_unix: u64,
) {
    let chunks = watch_chunks(frame.area(), confirm.is_some(), notice.is_some());

    // The tab bar lives in the header row the layout already had — the card's
    // geometry (and with it `priority_fields_fit`) is untouched by nav-shell.
    frame.render_widget(
        Paragraph::new(header_line(
            View::Queue,
            items.len(),
            wallet.policy,
            frame.area().width,
        )),
        chunks[0],
    );

    render_queue(frame, items, selected, now_unix, chunks[1]);

    // The card chunk exists only while a confirmation is open, so everything
    // after it shifts by one — see `watch_chunks`.
    let actions_idx = if confirm.is_some() {
        render_detail(frame, confirm, wallet.address, wallet.policy, chunks[2]);
        3
    } else {
        2
    };
    // The same fit the model gates approve on (`priority_fields_fit`), taken
    // from the very chunk the card is drawn into. With no card open there is
    // nothing to gate: `is_none_or` answers true and no chunk is consulted.
    let approve_ok =
        confirm.is_none_or(|c| card_priority_fits(c, wallet.address, wallet.policy, chunks[2]));
    render_actions(frame, confirm, approve_ok, now_unix, chunks[actions_idx]);

    if let Some(notice) = notice {
        frame.render_widget(Paragraph::new(notice_line(notice)), chunks[actions_idx + 1]);
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
    now_unix: u64,
    area: ratatui::layout::Rect,
) {
    let title = format!(" Queue — {} waiting for your decision ", items.len());
    let block = themed_block(if items.is_empty() { " Queue " } else { &title });
    if items.is_empty() {
        let empty = Paragraph::new("Queue is empty — waiting for approval requests…").block(block);
        frame.render_widget(empty, area);
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Collapsed to a strip (a card is open): one row of inner space cannot hold
    // both the heading and the item it heads, and the strip exists to show the
    // item. Between the two, the item wins.
    let collapsed = inner.height <= 1;
    let split = if collapsed {
        Layout::vertical([Constraint::Length(0), Constraint::Min(0)]).split(inner)
    } else {
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            // The two leading columns are the ones the selection bar occupies
            // on every data row; without them the header floats two left.
            format!(
                "  {}",
                queue_row("", "KIND", "AMOUNT", "", "RECIPIENT", "NETWORK", "EXPIRES")
            ),
            Style::new().fg(theme::faint()),
        )),
        split[0],
    );

    let rows: Vec<(String, bool)> = items
        .iter()
        .map(|s| {
            // Danger reads before the card is opened, so the marker carries it
            // and the whole row takes the alarm colour.
            let marker = if s.high_risk { '◆' } else { '●' };
            // Money first, kind second. A call CAN carry native value, and a
            // column that says only "contract" would hide it — the same rule
            // the card follows: a zero native value is not headlined, a
            // non-zero one always is. (`wei_to_eth` already carries the unit.)
            let amount = if format::is_zero_wei(&s.amount_wei) && s.kind == Kind::Call {
                "contract".to_owned()
            } else {
                // Shortened: the queue is scanned, and the exact figure waits on
                // the card, where the decision is actually made.
                format::short_eth(&s.amount_wei)
            };
            let text = queue_row(
                &marker.to_string(),
                kind_word(s),
                &amount,
                "→",
                &format::short_addr(&s.to),
                &format::network_name(s.chain_id),
                &time_left_word(s.not_after_unix, now_unix),
            );
            (text, s.high_risk)
        })
        .collect();
    let cursor = selected.min(items.len().saturating_sub(1));
    // The bar is drawn as content rather than through `highlight_symbol`,
    // because a highlight style repaints the whole row and would swallow the
    // amber that carries risk. Bar in accent, row in its own colour, bold on
    // the selected one — never inversion, which fights the light theme (§5).
    let rows: Vec<ListItem> = rows
        .into_iter()
        .enumerate()
        .map(|(i, (text, high_risk))| {
            let mut style = if high_risk {
                Style::new().fg(theme::high_risk())
            } else {
                Style::new()
            };
            if i == cursor {
                style = style.add_modifier(Modifier::BOLD);
            }
            let bar = if i == cursor {
                Span::styled(
                    "▌ ",
                    Style::new()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            ListItem::new(Line::from(vec![bar, Span::styled(text, style)]))
        })
        .collect();
    // A window around the cursor, kept by hand rather than by `ListState`:
    // the stateful widget brings `highlight_style` back with it, and that
    // style repaints the whole row — which is what swallowed the amber on a
    // selected high-risk item once already. The bar is content here, so the
    // scrolling has to be content too.
    let view_rows = usize::from(split[1].height);
    let (rows, hidden) = if view_rows == 0 || rows.len() <= view_rows {
        (rows, 0)
    } else {
        // Leave the last row for the marker, and keep the cursor inside.
        let shown = view_rows - 1;
        let start = cursor
            .saturating_sub(shown.saturating_sub(1))
            .min(rows.len().saturating_sub(shown));
        let hidden = rows.len() - shown;
        (rows[start..start + shown].to_vec(), hidden)
    };
    let mut rows = rows;
    if hidden > 0 {
        rows.push(ListItem::new(Span::styled(
            format!("  +{hidden} more — terminal too small"),
            Style::new().fg(theme::faint()),
        )));
    }
    frame.render_widget(List::new(rows), split[1]);
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
fn render_receive(frame: &mut Frame, pending: usize, wallet: Option<&str>, policy: Policy) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());
    frame.render_widget(
        Paragraph::new(header_line(
            View::Receive,
            pending,
            policy,
            frame.area().width,
        )),
        chunks[0],
    );

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

/// The Activity view: terminal outcomes newest-first — the local log merged
/// with the server's retained window (Stage 7). Pure display: nothing here
/// signs or gates. Addresses are SHORTENED (`format::short_addr`) — the one
/// display list ТЗ §4.1 allows it on; signing surfaces render in full. Rows
/// that do not fit end with an explicit "+N more" marker, never a silent
/// clip (the Stage-5 budget lesson: exact-fit vs marker split, one budget).
fn render_activity(frame: &mut Frame, pending: usize, model: &Model, now_unix: u64) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());
    frame.render_widget(
        Paragraph::new(header_line(
            View::Activity,
            pending,
            model.policy(),
            frame.area().width,
        )),
        chunks[0],
    );

    let block = themed_block(" Activity ");
    let inner = block.inner(chunks[1]);
    let height = usize::from(inner.height);

    let filter = model.history_filter();
    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        format!("filter: {}  [f cycles]", filter.label()),
        theme::label_style(),
    ))];
    let note_lines = usize::from(model.history_note().is_some());

    let rows: Vec<&HistoryEntry> = model
        .history()
        .iter()
        .filter(|e| filter.admits(e.state))
        .collect();
    if rows.is_empty() {
        let text = if model.history().is_empty() {
            "no activity yet".to_owned()
        } else {
            format!("no {} outcomes under this filter", filter.label())
        };
        lines.push(Line::from(Span::styled(text, theme::label_style())));
    } else {
        // The budget is computed ONCE from what is already reserved (header +
        // footer note); the loop below never re-subtracts it. Exact fit shows
        // everything; one over shows budget−1 rows + an honest marker.
        let budget = height.saturating_sub(lines.len() + note_lines);
        let total = rows.len();
        if total <= budget {
            lines.extend(rows.iter().map(|e| activity_line(e, now_unix)));
        } else if let Some(kept) = budget.checked_sub(1) {
            lines.extend(rows[..kept].iter().map(|e| activity_line(e, now_unix)));
            lines.push(Line::from(Span::styled(
                format!("+{} more (history lives in the local log)", total - kept),
                theme::label_style(),
            )));
        }
        // budget == 0: the block has no visible rows at all — nothing is
        // being clipped in silence, the whole list is out of view.
    }
    if let Some(note) = model.history_note() {
        lines.push(Line::from(Span::styled(
            note.to_owned(),
            theme::high_risk_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), chunks[1]);
}

/// One outcome row. A rich record carries the card data written at decision
/// time; a server-only record renders an honest "(details not recorded)"
/// instead of fabricated columns.
fn activity_line(entry: &HistoryEntry, now_unix: u64) -> Line<'static> {
    let (word, color) = match entry.state {
        OutcomeState::Executed => ("approved", theme::approve()),
        OutcomeState::Denied => ("rejected", theme::reject()),
        OutcomeState::Expired => ("expired", theme::high_risk()),
        OutcomeState::Failed => ("failed", theme::reject()),
    };
    let mut spans = vec![
        Span::styled(
            format!("{:>7}  ", age_label(now_unix, entry.unix)),
            theme::label_style(),
        ),
        Span::styled(format!("{word:<8}"), Style::new().fg(color)),
    ];
    match (&entry.to, &entry.amount_wei) {
        // A scan surface, like the queue: shortened amount beside a shortened
        // address. What was actually signed lives in the core's audit log.
        (Some(to), Some(amount)) => spans.push(Span::raw(format!(
            " {} → {}",
            format::short_eth(amount),
            format::short_addr(to)
        ))),
        _ => spans.push(Span::styled(
            " (details not recorded)".to_owned(),
            theme::label_style(),
        )),
    }
    if let Some(tx) = &entry.tx_hash {
        spans.push(Span::styled(
            format!("  tx {}", format::short_addr(tx)),
            theme::label_style(),
        ));
    }
    if let Some(reason) = &entry.reason {
        spans.push(Span::styled(format!("  {reason}"), theme::label_style()));
    }
    Line::from(spans)
}

/// Compact "how long ago" for a history row (whole units, floor).
fn age_label(now_unix: u64, unix: u64) -> String {
    let s = now_unix.saturating_sub(unix);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

/// The rows the confirmation banner claims: two borders plus its two lines.
const ACK_BANNER_ROWS: u16 = 4;

/// Width of the Dashboard's identity column. Fixed rather than proportional:
/// it holds a handful of short lines whose longest is a version row —
/// `VERSION_LABEL_WIDTH` plus a `v` and the number — so a share of the width
/// would only take room from the balances beside it.
const IDENTITY_COL: u16 = 22;

/// The most rows the balance panel may take INSIDE its borders, however long the
/// list gets. Eight holds three chains, three registry tokens, the staleness
/// note and a `+N more` marker — the shape of a wallet with a token or two per
/// chain (Captain, 2026-08-10). Past that the list is asked to fit, because the
/// panel shares the column with positions, and a balance list free to grow would
/// push them off the screen entirely.
const BALANCE_ROWS_MAX: u16 = 8;

/// Rows the balance panel asks for: its two borders plus the rows it will
/// actually draw, capped by [`BALANCE_ROWS_MAX`].
///
/// Measured off the built rows, never off the number of assets. A long reason
/// wraps onto two rows at an ordinary terminal width, and counting entries
/// budgeted one row for it — which put real balances behind `+N more` at four
/// assets, nowhere near the ceiling. That is the defect Р7 exists to remove,
/// arriving through a second door, and the door is the same one the card walked
/// through before it: two counts of the same thing (round-6 blocker).
fn balance_panel_rows(entries: &[Vec<Line<'static>>], stale: bool) -> u16 {
    let rows: usize = entries.iter().map(Vec::len).sum();
    // The list comes off the wire, so its length is the core's to choose. A list
    // longer than a u16 asks for more rows than any terminal has; saturating
    // here costs nothing, because the cap below is what the panel actually takes.
    let rows = u16::try_from(rows).unwrap_or(u16::MAX);
    let inner = rows.saturating_add(u16::from(stale)).min(BALANCE_ROWS_MAX);
    inner + 2
}

/// Every row the balance panel will draw, **grouped by the asset it is about**
/// and already wrapped to `width`.
///
/// One group per entry, for two reasons that only a group boundary can give:
/// truncation cuts between assets instead of through the middle of one, and
/// `+N more` counts assets rather than terminal rows. Built once and handed to
/// both the height arithmetic and the renderer — the same single-source contract
/// [`priority_lines`] keeps for the card, and for the same reason.
fn balance_entries(model: &Model, width: usize) -> Vec<Vec<Line<'static>>> {
    let wrap = |text: String, style: Style| {
        let mut group: Vec<Line<'static>> = Vec::new();
        push_wrapped(&mut group, width, text, style);
        group
    };
    match model.wallet_context() {
        Some(ctx) if !ctx.balances.is_empty() || !ctx.unavailable.is_empty() => {
            // Warnings first, and deliberately so: truncation cuts from the end,
            // so what goes behind "+N more" is a number, never the line saying a
            // number is missing. A hidden warning reads as "all is well" — the
            // same rank the staleness line has held since Ф-4. Whoever adds a
            // third category of row here inherits that ordering: the sequence
            // below IS the priority, and the truncation downstream trusts it.
            let unread = ctx.unavailable.iter().map(|u| {
                wrap(
                    format!(
                        "  {}  {}  {}",
                        format::network_name(u.chain_id),
                        asset_symbol(&u.symbol),
                        unavailable_reason(&u.reason)
                    ),
                    theme::high_risk_style(),
                )
            });
            let held = ctx.balances.iter().map(|b| {
                // The core rendered the amount and named the unit; the panel
                // states each once. It used to append `b.symbol` to an amount
                // that already carried "ETH", which is what made the first
                // screen read `0.01 ETH ETH` — the fix then was to drop the
                // symbol, because every row was ether. Now a row can be USDC,
                // so the symbol is what the formatter is told to state.
                //
                // A token also shows its contract, shortened. The symbol does
                // not identify it — native USDC and bridged USDC.e share one,
                // and a registry that named a token "ETH" would otherwise draw
                // a row indistinguishable from the chain's own coin on the very
                // panel whose job is to say what is held.
                let mut row = format!(
                    "  {}  {}",
                    format::network_name(b.chain_id),
                    format::short_amount(&b.balance_formatted, &asset_symbol(&b.symbol))
                );
                if !b.token_address.is_empty() {
                    row.push_str("  ");
                    row.push_str(&format::short_addr(&b.token_address));
                }
                wrap(row, theme::value_style())
            });
            unread.chain(held).collect()
        }
        Some(_) => vec![wrap(
            "  no balances reported".to_owned(),
            theme::label_style(),
        )],
        None => vec![wrap(
            "  balance unavailable".to_owned(),
            theme::label_style(),
        )],
    }
}

/// An asset symbol as it may be drawn: printable ASCII, or a stand-in.
///
/// The same judgement [`parse_stated_version`] makes about a version string, for
/// the same reason: a bidirectional override is not a control character, passes
/// a control-character test, and reorders the glyphs around it — on the panel
/// whose only job is to say truthfully what this wallet holds. The symbol comes
/// from an operator's registry entry, so it is the one string on this panel a
/// human types by hand.
fn asset_symbol(symbol: &str) -> String {
    if symbol.is_empty() || !symbol.chars().all(|c| c.is_ascii_graphic()) {
        return "?".to_owned();
    }
    symbol.to_owned()
}

/// Columns reserved for a version label, so the numbers line up under one
/// another. `console` is the longest of the three, and two spaces after it keep
/// the numbers off the word.
const VERSION_LABEL_WIDTH: usize = 9;

/// What the running wallet image states about the layers the console cannot
/// ask directly.
///
/// The console's own version is absent from this by design: the binary knows it
/// at compile time and needs nobody's word for it. These two it does need, and
/// `None` means the image said nothing — which is the ordinary case outside it.
#[derive(Clone, Copy, Default)]
pub struct Versions<'a> {
    wallet: Option<&'a str>,
    core: Option<&'a str>,
}

/// One version label as the image states it, or `None` when it states nothing
/// usable.
///
/// Blank is not a version, and a control character would tear the frame this
/// panel is drawn inside, so both read as absent rather than as a value. A
/// leading `v` is dropped because the two sources disagree on shape — the
/// wallet's number comes from a manifest (`0.9.3`), the core's from an image
/// tag (`v0.4.1`) — and the panel states one shape regardless of which side of
/// the build a number arrived from.
fn parse_stated_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // The `v` comes off FIRST, and then the result is judged. The other order
    // passes a bare `v` through as an empty version, and the panel draws
    // `wallet   v` — a label for a number, with no number.
    let stated = trimmed.strip_prefix('v').unwrap_or(trimmed);
    // Printable ASCII, and nothing else. This is stricter than rejecting
    // control characters, and deliberately so: a bidirectional override is not
    // a control character, passes that test, and reorders the glyphs around it
    // — on a panel whose only job is to say truthfully what is running. Every
    // version scheme this project uses is `0-9 A-Z a-z . - +`, so nothing
    // legitimate is turned away.
    if stated.is_empty() || !stated.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    Some(stated.to_owned())
}

/// Split from [`parse_stated_version`] so the judgement above is a pure
/// function: proving that a blank or a torn value reads as absent must not
/// require mutating the environment of a parallel test binary.
fn stated_version(name: &str) -> Option<String> {
    parse_stated_version(&std::env::var(name).ok()?)
}

/// The versions of the image this console is running inside, read once.
///
/// Read once because an image cannot relabel itself mid-run while the renderer
/// asks on every frame. Split from [`version_lines`] for the reason `theme`
/// splits its own env read: the shape of the panel, degradation included, stays
/// a pure function that no environment can move.
///
/// In the test binary this is empty, and that is not a workaround. The test
/// binary is not the wallet image, so the honest answer there is "the image
/// said nothing" — and a developer who happens to have `RUSTOK_WALLET_VERSION`
/// exported would otherwise get a red test for a reason unrelated to the code.
/// The populated shape is covered directly instead, by passing values in.
pub fn image_versions() -> Versions<'static> {
    static STATED: std::sync::OnceLock<(Option<String>, Option<String>)> =
        std::sync::OnceLock::new();
    let (wallet, core) = STATED.get_or_init(|| {
        (
            stated_version("RUSTOK_WALLET_VERSION"),
            stated_version("RUSTOK_CORE_VERSION"),
        )
    });
    Versions {
        wallet: wallet.as_deref(),
        core: core.as_deref(),
    }
}

fn version_line(label: &str, version: &str) -> String {
    format!("{label:<width$}v{version}", width = VERSION_LABEL_WIDTH)
}

/// The version lines of the identity panel.
///
/// Inside the wallet image all three layers are named, because all three can
/// move independently and a human looking at this screen is usually asking
/// which ones did. Outside it — a bare `cargo run`, or the console image on its
/// own — the panel says exactly what it said before: one line, for the binary
/// that knows itself.
///
/// A missing source prints nothing at all, never `unknown`. Absence is not a
/// value, and a word standing where a number belongs invites the reader to
/// treat it as one.
/// A version too long for the column is cut with the same marker every other
/// overflow in this file carries, rather than clipped silently by the renderer.
/// The fence is tied to [`IDENTITY_COL`] instead of a written-out number: the
/// column and the cell inside it cannot drift apart if only one of them exists.
fn version_lines(versions: Versions<'_>) -> Vec<String> {
    const CELL: usize = IDENTITY_COL as usize - 2; // the two borders
    let console = env!("CARGO_PKG_VERSION");
    let mut lines = Vec::with_capacity(3);
    if versions.wallet.is_none() && versions.core.is_none() {
        lines.push(format!("console v{console}"));
    } else {
        if let Some(wallet) = versions.wallet {
            lines.push(version_line("wallet", wallet));
        }
        lines.push(version_line("console", console));
        if let Some(core) = versions.core {
            lines.push(version_line("core", core));
        }
    }
    lines
        .into_iter()
        .map(|line| clamp_cell(&line, CELL))
        .collect()
}

/// Who this wallet is: the product, the versions of what is actually running,
/// and which address is loaded (design v2, mockup states 1–2).
///
/// Until 0.3.1 this printed one number — the crate's own `CARGO_PKG_VERSION` —
/// and the comment here argued the case for that: the number a user installs
/// belongs to the wallet image, it was not passed in at build time, and
/// printing a figure the program cannot verify is the same class of claim as a
/// mode without its origin (В-3).
///
/// The argument was right, and this circle removed its premise instead of
/// overruling it. The build now states those numbers into the image, and the
/// publishing workflow refuses to produce an artifact whose number disagrees
/// with its manifest. So the panel no longer prints a figure it cannot check —
/// it prints what the artifact says about itself, which is a different claim,
/// and outside that artifact it goes quiet rather than guessing.
fn render_identity(
    frame: &mut Frame,
    address: Option<&str>,
    versions: Versions<'_>,
    area: ratatui::layout::Rect,
) {
    let block = themed_block("");
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "RUSTOK",
            Style::new()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" WALLET", Style::new().add_modifier(Modifier::BOLD)),
    ])];
    lines.extend(
        version_lines(versions)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::new().fg(theme::faint())))),
    );
    if let Some(address) = address {
        lines.push(Line::from(Span::styled(
            format::short_addr(address),
            Style::new().fg(theme::accent_bright()),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The one-time invitation to confirm autonomy (design §3).
///
/// It is drawn **only** while there is something to confirm, for the same
/// reason the card no longer holds rows it is not using: a permanent strip for
/// a once-in-a-wallet action is a standing reserve. A supervised wallet has no
/// autonomy to acknowledge, so it is never asked.
///
/// The alarm colour is the same one the header spends on this one state — the
/// banner is where that state says what to do about itself.
fn render_ack_banner(frame: &mut Frame, area: ratatui::layout::Rect) {
    let block = Block::bordered()
        .border_style(Style::new().fg(theme::high_risk()))
        .title(Line::from(Span::styled(
            " Autonomous mode unconfirmed ",
            Style::new()
                .fg(theme::high_risk())
                .add_modifier(Modifier::BOLD),
        )));
    let body = Paragraph::new(vec![
        Line::from("Every send queues and waits for you."),
        Line::from(vec![
            Span::styled("[c]", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" — confirm autonomy "),
            Span::styled("(requires PIN)", Style::new().fg(theme::faint())),
        ]),
    ])
    .block(block);
    frame.render_widget(body, area);
}

/// Column widths of the queue table — the ONE place they live.
///
/// The header and the data rows were two independently hand-built strings and
/// they drifted: `RECIPIENT` ended up over the arrow, two columns left of the
/// address it names. Both now go through [`queue_row`], so a width cannot move
/// in one without moving in the other.
const Q_MARKER: usize = 2;
const Q_KIND: usize = 8;
const Q_AMOUNT: usize = 19;
/// Money is read down the right edge of its column (design §4, §5), so the
/// amount is the one cell that aligns right. The column keeps its last
/// character as a gap, or the number would touch the arrow.
const Q_AMOUNT_CELL: usize = Q_AMOUNT - 1;
const Q_ARROW: usize = 2;
const Q_RECIPIENT: usize = 15;
const Q_NETWORK: usize = 11;

/// One line of the queue table, header or data. The arrow has its own cell so
/// the `RECIPIENT` heading sits over the address rather than over the arrow.
///
/// Every fixed cell is clamped to its width: a value wider than its column would
/// otherwise shove everything after it sideways, out from under the headings —
/// which is exactly what a long amount did on the first live run.
fn queue_row(
    marker: &str,
    kind: &str,
    amount: &str,
    arrow: &str,
    recipient: &str,
    network: &str,
    expires: &str,
) -> String {
    let marker = clamp_cell(marker, Q_MARKER);
    let kind = clamp_cell(kind, Q_KIND);
    let amount = clamp_cell(amount, Q_AMOUNT_CELL);
    let arrow = clamp_cell(arrow, Q_ARROW);
    let recipient = clamp_cell(recipient, Q_RECIPIENT);
    let network = clamp_cell(network, Q_NETWORK);
    // `expires` is last and has no column after it to disturb, so it is not
    // clamped — the terminal's own edge is its limit.
    format!(
        "{marker:<Q_MARKER$}{kind:<Q_KIND$}{amount:>Q_AMOUNT_CELL$} {arrow:<Q_ARROW$}\
{recipient:<Q_RECIPIENT$}{network:<Q_NETWORK$}{expires}"
    )
}

/// Trim a cell to its column, marking that it was trimmed.
///
/// Counted in CHARACTERS, not bytes: these rows carry `▌ ● ◆ → …`, and a byte
/// ruler measures a different table than the one the terminal draws.
fn clamp_cell(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    // The amount may already carry its own `…` from `short_eth`, and the cut can
    // land right on it. One marker is the whole message — "not all of it is
    // here" — so a second is dropped rather than stacked.
    format!("{}…", kept.trim_end_matches('…'))
}

/// How long is left before this item expires, for the queue's last column.
///
/// The mockup asked for AGE, which the wire cannot produce: a summary carries
/// its deadline, never its birth. Time LEFT is both derivable and the thing
/// that matters when triaging a queue — how long the human has, not how long
/// it has sat (Reviewer, round 10). Saturating like the card's countdown: a
/// deadline already past reads as expired, never as a wrapped-around eternity.
fn time_left_word(not_after_unix: u64, now_unix: u64) -> String {
    match seconds_left(not_after_unix, now_unix) {
        0 => "expired".to_owned(),
        s if s < 60 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

/// Why this payment is waiting, derived from the pair (mode, origin) — §4.
///
/// The wire carries no reason field and does not need one: within a single
/// wallet the reason is the same for every parked item, so a per-item field
/// would be the same string repeated. Only the unconfirmed case is
/// alarm-coloured, the same rule the header follows applied to the same fact.
fn parking_reason(policy: Policy) -> (&'static str, Style) {
    match (policy.mode, policy.origin) {
        (PolicyMode::Autonomous, PolicyOrigin::Provisioned) => (
            "parked: autonomy unconfirmed — confirm on the Dashboard",
            Style::new().fg(theme::high_risk()),
        ),
        (PolicyMode::Autonomous, PolicyOrigin::Acknowledged) => (
            "parked until mode confirmation — decided by you",
            theme::label_style(),
        ),
        _ => ("waiting for your decision", theme::label_style()),
    }
}

/// The mode switcher (spec §2.4), centred over the Dashboard.
///
/// Same behaviour family as the card's high-risk prompt — on top, masked,
/// nothing stored. Two stages under one box: the pick list, then the PIN row
/// once a pick is confirmed. The autonomy disclaimer renders whenever the
/// selector stands on `autonomous`, in BOTH stages: the human reads it before
/// the PIN and while typing it, never after.
fn render_mode_switch(frame: &mut Frame, switch: &ModeSwitch, current: PolicyMode) {
    const DISCLAIMER: [&str; 3] = [
        "Autonomy has no spending limits in this build:",
        "the ceiling is the wallet balance. An approval the agent",
        "signs is not capped and outlives the agent; only a separate transaction revokes it.",
    ];
    let selected = switch.selected();
    let mut lines: Vec<Line> = Vec::with_capacity(10);
    for mode in [
        PolicyMode::ReadOnly,
        PolicyMode::Supervised,
        PolicyMode::Autonomous,
    ] {
        let word = mode.wire_word().unwrap_or("?");
        let marker = if mode == selected { "▸ " } else { "  " };
        let tag = if mode == current { "  (current)" } else { "" };
        let style = if mode == selected {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            theme::label_style()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{word}{tag}"),
            style,
        )));
    }
    if selected == PolicyMode::Autonomous {
        lines.push(Line::default());
        for row in DISCLAIMER {
            lines.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme::high_risk()),
            )));
        }
    }
    lines.push(Line::default());
    match switch.pin_len() {
        Some(pin_len) => {
            lines.push(Line::from(format!("PIN: {}", "●".repeat(pin_len))));
            lines.push(Line::from(Span::styled(
                "enter — apply · esc — cancel",
                Style::new().fg(theme::faint()),
            )));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "↑/↓ — choose · enter — continue (PIN) · esc — cancel",
                Style::new().fg(theme::faint()),
            )));
        }
    }

    let area = frame.area();
    let width = 64.min(area.width);
    #[allow(clippy::cast_possible_truncation)] // bounded: at most 10 lines + 2 border rows
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = ratatui::layout::Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(theme::high_risk()))
        .title(Line::from(Span::styled(
            " Wallet mode ",
            Style::new()
                .fg(theme::high_risk())
                .add_modifier(Modifier::BOLD),
        )));
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Word an unread asset for the human, from the reason the core sent.
///
/// The three the core knows are separated because the answer to each differs:
/// no RPC is the operator's configuration, a failed call is the network and may
/// pass next minute, and a revert means the address in the registry is not the
/// ERC-20 it was said to be — "try later" would be advice in the wrong
/// direction. A word this console does not know is shown as it came: the core
/// may learn a reason before the console does, and inventing a translation for
/// it would be worse than quoting it.
fn unavailable_reason(reason: &str) -> String {
    match reason {
        "no_rpc_configured" => "not queried — no RPC".to_owned(),
        "rpc_call_failed" => "not read — RPC call failed".to_owned(),
        "call_reverted" => "not read — call reverted, check the registry".to_owned(),
        other => format!("not read — {other}"),
    }
}

/// The Dashboard: the wallet's balances (from `context` — each chain's native
/// coin and the registry tokens it holds), DeFi positions (the `positions`
/// read-op), and the "waiting for you" count. Pure display — nothing here signs
/// or gates; the values render **verbatim** (`extra` are display strings by
/// canon §3.8 — including the literal `"∞"`).
///
/// Honesty rules: a failed balance refresh flags the block as possibly stale
/// (never silently shows old data as fresh); rows that do not fit end with an
/// explicit "+N more" marker, never a silent clip; and an asset the core could
/// not read is drawn as a warning rather than left out, so a missing row means
/// zero and nothing else.
fn render_dashboard(frame: &mut Frame, pending: usize, model: &Model, versions: Versions<'_>) {
    let policy = model.policy();
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());
    frame.render_widget(
        Paragraph::new(header_line(
            View::Dashboard,
            pending,
            policy,
            frame.area().width,
        )),
        chunks[0],
    );

    // The banner takes its rows from the body, not from the header: the mode
    // phrase must stay on screen in every state, the invitation only in one.
    let body = if policy.awaits_acknowledgment() {
        let split = Layout::vertical([Constraint::Length(ACK_BANNER_ROWS), Constraint::Min(0)])
            .split(chunks[1]);
        render_ack_banner(frame, split[0]);
        split[1]
    } else {
        chunks[1]
    };

    // Identity on the left, content on the right (design v2). The identity
    // column is fixed: it holds a handful of short lines whose longest is a
    // version row, so giving it a share of the width would only take room from
    // the balances.
    let cols =
        Layout::horizontal([Constraint::Length(IDENTITY_COL), Constraint::Min(0)]).split(body);
    render_identity(frame, model.wallet_address(), versions, cols[0]);

    // The balance rows are built HERE, before the split that gives the panel its
    // height, and the same vector is what gets drawn below. The width they wrap
    // to is already known — a vertical split does not change it — and the height
    // is measured off the rows themselves. Two counts of the same thing is the
    // mistake this panel and the card above it have both paid for once.
    let balance_width = usize::from(cols[1].width.saturating_sub(2));
    let balance_content = balance_entries(model, balance_width);

    let panels = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(balance_panel_rows(&balance_content, model.context_stale())),
        Constraint::Min(0),
    ])
    .split(cols[1]);

    // ── Waiting for you — the reason this console exists comes first.
    let queue_block = themed_block(" Queue ");
    let width = usize::from(queue_block.inner(panels[0]).width);
    let waiting = if policy.awaits_acknowledgment() {
        format!("Waiting: {pending} — all parked, mode unconfirmed")
    } else if pending == 0 {
        "Waiting for you: nothing pending".to_owned()
    } else {
        format!("Waiting for you: {pending} pending — press a")
    };
    let waiting_style = if pending == 0 {
        theme::label_style()
    } else {
        theme::high_risk_style()
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    push_wrapped(&mut lines, width, waiting, waiting_style);
    frame.render_widget(Paragraph::new(lines).block(queue_block), panels[0]);

    let block = themed_block(" balance ");
    let inner = block.inner(panels[1]);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // ── Balance (from `context`): every asset the wallet holds, and above them
    // the ones it could not read at all. The rows were built before the split
    // above; this loop only decides how many of them fit.
    //
    // The staleness line is not a balance row — it is the line that says the
    // balance rows may be wrong. It outranks them: a reserved row keeps it out
    // of the truncation, so a wallet with many chains cannot quietly drop the
    // one line warning that the numbers above it are stale.
    let stale = model.context_stale();
    let budget = height.saturating_sub(usize::from(stale));
    // The order the entries arrive in IS their priority: this loop drops from
    // the tail, and `balance_entries` puts the warnings at the head so what goes
    // behind the marker is a number, never the line saying a number is missing.
    // Reordering there silently changes what this loop protects.
    let total = balance_content.len();
    let mut used = 0usize;
    for (i, group) in balance_content.into_iter().enumerate() {
        let remaining = total - i;
        // Reserve one row for the "+N more" marker — except for the last entry,
        // which may take the final row itself (an exact fit shows everything,
        // no marker). Same arithmetic as the positions panel below.
        let reserve = usize::from(remaining > 1);
        if used + group.len() + reserve > budget {
            // What was hidden is stated rather than silently cut, and the reason
            // is stated too: a panel that has hit its own ceiling will not show
            // more however the window is dragged, and "terminal too small" would
            // send the human off to resize for nothing.
            let cause = if inner.height >= BALANCE_ROWS_MAX {
                "panel is full"
            } else {
                "terminal too small"
            };
            push_wrapped(
                &mut lines,
                width,
                format!("  +{remaining} more — {cause}"),
                theme::label_style(),
            );
            break;
        }
        used += group.len();
        lines.extend(group);
    }
    if stale {
        push_wrapped(
            &mut lines,
            width,
            "  balance may be stale — refresh failed".to_owned(),
            theme::high_risk_style(),
        );
    }

    frame.render_widget(Paragraph::new(lines).block(block), panels[1]);

    let block = themed_block(" positions ");
    let inner = block.inner(panels[2]);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // ── Positions (tri-state: loading / loaded / unavailable).
    match model.positions() {
        Positions::NotYet => push_wrapped(
            &mut lines,
            width,
            "  loading positions…".to_owned(),
            theme::label_style(),
        ),
        Positions::Unavailable => push_wrapped(
            &mut lines,
            width,
            "  positions unavailable".to_owned(),
            theme::label_style(),
        ),
        Positions::Loaded(list) if list.is_empty() => push_wrapped(
            &mut lines,
            width,
            "  no DeFi positions".to_owned(),
            theme::label_style(),
        ),
        Positions::Loaded(list) => {
            // Rows still available for position lines: the panel height
            // minus what the blocks above already used. Inside the loop only
            // `used` — rows added BY THIS LOOP — is compared against it: the
            // Gate-2 blocker compared the ever-growing `lines.len()`, which
            // still contains the header rows the budget had already
            // subtracted, so the header was counted twice and the marker cut
            // positions that actually fit.
            let budget = height.saturating_sub(lines.len());
            let mut used = 0usize;
            for (i, p) in list.iter().enumerate() {
                let extra: String = p
                    .extra
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let mut row = format!(
                    "  {} · {} {} {}",
                    p.protocol, p.balance_formatted, p.asset_symbol, p.asset_name
                );
                if !extra.is_empty() {
                    row.push_str("  —  ");
                    row.push_str(&extra);
                }
                let mut rendered = Vec::new();
                push_wrapped(&mut rendered, width, row, theme::value_style());
                let remaining = list.len() - i;
                // Reserve one row for the "+N more" marker — except for the
                // last position, which may take the final row itself (an
                // exact fit shows everything, no marker).
                let reserve = usize::from(remaining > 1);
                if used + rendered.len() + reserve > budget {
                    push_wrapped(
                        &mut lines,
                        width,
                        format!("  +{remaining} more — terminal too small"),
                        theme::label_style(),
                    );
                    break;
                }
                used += rendered.len();
                lines.append(&mut rendered);
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).block(block), panels[2]);
}

/// The card's priority lines — every field except `raw_data` — pre-wrapped to
/// `width` display cells, so one logical line is one visual row and the height
/// arithmetic downstream is exact. One source for the renderer AND for
/// [`priority_fields_fit`]: the approve gate can never disagree with what is
/// actually drawn.
fn priority_lines(
    confirm: &Confirm,
    from: Option<&str>,
    policy: Policy,
    width: usize,
) -> Vec<Line<'static>> {
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
        // Named, not numbered — the same word the queue and the balance use for
        // the same network. The label moves with the value: an unknown chain
        // renders as `chain 42161`, and `chain  chain 42161` would read as a
        // stutter.
        format!("network  {}", format::network_name(card.chain_id)),
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
    // Why this one is waiting (§4) — after the risk warnings, before the PIN
    // prompt and the deadline. A priority field on purpose: a human who cannot
    // see why the payment stopped cannot tell "the wallet is asking me" from
    // "the wallet is broken".
    let (reason, reason_style) = parking_reason(policy);
    push_wrapped(&mut lines, width, reason.to_owned(), reason_style);
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
fn card_priority_fits(
    confirm: &Confirm,
    from: Option<&str>,
    policy: Policy,
    area: ratatui::layout::Rect,
) -> bool {
    let inner = Block::bordered().inner(area);
    priority_lines(confirm, from, policy, usize::from(inner.width)).len()
        <= usize::from(inner.height)
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
pub fn priority_fields_fit(
    confirm: &Confirm,
    from: Option<&str>,
    policy: Policy,
    width: u16,
    height: u16,
) -> bool {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let chunks = watch_chunks(area, true, false);
    card_priority_fits(confirm, from, policy, chunks[2])
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
    policy: Policy,
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

    let mut lines = priority_lines(confirm, from, policy, width);
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
        AuthOutcome, Card, ContextOutcome, DecodedCall, GetOutcome, Kind, Risk, WalletContext,
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
        draw_rows_with(model, w, h, now_unix, Versions::default())
    }

    /// The whole screen, with what the image states about itself passed in.
    ///
    /// This is the seam that lets an ordinary `cargo test` prove the panel is
    /// **wired**, not merely shaped: a mutation that drops the versions on the
    /// way from `render` to the identity panel fails here, without a script and
    /// without touching the environment of a parallel test binary.
    fn draw_rows_with(
        model: &Model,
        w: u16,
        h: u16,
        now_unix: u64,
        versions: Versions<'_>,
    ) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, model, now_unix, versions))
            .unwrap();
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
        terminal
            .draw(|f| render(f, model, NOW, Versions::default()))
            .unwrap();
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

    /// The Captain's question, as a test: the bottom block must not hold rows
    /// while there is nothing in it. The card earns its space in the moment of
    /// a decision and gives it back when the decision is over — the property
    /// `watch_chunks` protects (rows for risk warnings and the PIN prompt on a
    /// 24-row terminal) is needed only while the card is open, and by then the
    /// queue has already collapsed to a strip and freed them.
    #[test]
    fn a_closed_card_reserves_no_rows() {
        let mut model = Model::default();
        to_watching(
            &mut model,
            (0..18)
                .map(|i| {
                    summary(
                        &format!("{i:08}-0000-0000-0000-000000000000"),
                        "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                        "1000000000000000000",
                        false,
                    )
                })
                .collect(),
        );
        let rows = draw_rows(&model, 80, 24);
        let screen = rows.join("\n");
        assert!(
            !screen.contains("Card"),
            "no card block while none is open:\n{screen}"
        );
        assert!(
            !screen.contains("press enter to see the full card"),
            "the hint belongs in the navigation row, not in six reserved rows:\n{screen}"
        );

        // And the rows it used to hold go to the list.
        let listed = rows.iter().filter(|r| r.contains("0x8b3E")).count();
        assert!(
            listed >= 18,
            "every waiting item fits once the reserve is gone, saw {listed}:\n{screen}"
        );
    }

    /// The reverse side: with a card open the protected geometry is unchanged —
    /// the queue collapses to a strip and the card takes the rest.
    #[test]
    fn an_open_card_still_gets_its_rows() {
        let mut model = Model::default();
        to_watching(
            &mut model,
            vec![summary(
                "00000000-0000-0000-0000-000000000000",
                "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                "1000000000000000000",
                false,
            )],
        );
        model.update(Msg::Open);
        model.update(Msg::Reply(Reply::Get(GetOutcome::Card(card(
            "00000000-0000-0000-0000-000000000000",
            NOW + 300,
            false,
        )))));
        let screen = draw_rows(&model, 80, 24).join("\n");
        assert!(
            screen.contains("Card"),
            "an open card must be drawn:\n{screen}"
        );
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
                unavailable: vec![],
                allowed_chains: vec![1],
                policy: Default::default(),
            },
        )))));
        model.update(Msg::View(crate::app::View::Queue)); // Stage-5 home is Dashboard
        model.update(Msg::Tick);
        model.update(Msg::Reply(Reply::List(items)));
    }

    /// A session whose `context` answered ok with an EMPTY address string —
    /// distinct from a degraded context: `parse_context` rejects a missing
    /// address but passes `""` through (T2).
    /// [`to_watching`] with an explicit policy — the header states the pair, so
    /// its tests need to set it.
    fn to_watching_with_policy(model: &mut Model, policy: Policy) {
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
                address: WALLET.to_owned(),
                balances: vec![],
                unavailable: vec![],
                allowed_chains: vec![1],
                policy,
            },
        )))));
        model.update(Msg::Tick);
        model.update(Msg::Reply(Reply::List(vec![])));
    }

    fn policy_of(mode: PolicyMode, origin: PolicyOrigin) -> Policy {
        Policy { mode, origin }
    }

    /// Q5: the mode is loud at every use. It is stated on every screen, not
    /// only the one that can act on it.
    #[test]
    fn the_header_states_the_wallets_autonomy_on_every_screen() {
        for view in [
            crate::app::View::Dashboard,
            crate::app::View::Queue,
            crate::app::View::Receive,
            crate::app::View::Activity,
        ] {
            let mut model = Model::default();
            to_watching_with_policy(
                &mut model,
                policy_of(PolicyMode::Autonomous, PolicyOrigin::Provisioned),
            );
            model.update(Msg::View(view));
            let header = draw_rows(&model, 80, 24)[0].clone();
            assert!(
                header.contains("unconfirmed"),
                "{view:?} must state it too: {header}"
            );
        }
    }

    /// The Reviewer's criterion, and the whole point of the phrase: the alarm
    /// colour marks exactly one state — the one where the human's expectation
    /// and the wallet's behaviour come apart. Acknowledged autonomy is the
    /// human's own choice, not a warning; painting it red would spend the
    /// signal. Text alone cannot catch this — a swap of `high_risk()` for
    /// `accent()` renders fine and reads fine.
    #[test]
    fn the_alarm_colour_marks_only_unacknowledged_autonomy() {
        let alarm = theme::high_risk();
        for (mode, origin, needle, expect_alarm) in [
            (
                PolicyMode::Autonomous,
                PolicyOrigin::Provisioned,
                "unconfirmed",
                true,
            ),
            (
                PolicyMode::Autonomous,
                PolicyOrigin::Acknowledged,
                "confirmed",
                false,
            ),
            (
                PolicyMode::Supervised,
                PolicyOrigin::Provisioned,
                "manual",
                false,
            ),
        ] {
            let mut model = Model::default();
            to_watching_with_policy(&mut model, policy_of(mode, origin));
            let fgs = row_fgs_containing(&model, 80, 24, needle);
            assert_eq!(
                fgs.contains(&alarm),
                expect_alarm,
                "{mode:?}/{origin:?} alarm-coloured? expected {expect_alarm}"
            );
        }
    }

    /// Text modifiers on the first row containing `needle`. The design says the
    /// selection is a marker plus bold and **not** inversion (inversion fights
    /// the light theme) — a property no colour or text check can hold.
    fn row_mods_containing(model: &Model, w: u16, h: u16, needle: &str) -> Modifier {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, model, NOW, Versions::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..h {
            let text: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
            if text.contains(needle) {
                return (0..w).fold(Modifier::empty(), |acc, x| {
                    acc | buffer[(x, y)].style().add_modifier
                });
            }
        }
        Modifier::empty()
    }

    fn queued(model: &mut Model, policy: Policy) {
        to_watching_with_policy(model, policy);
        model.update(Msg::Reply(Reply::List(vec![
            {
                let mut s = summary(
                    "00000000-0000-0000-0000-000000000000",
                    "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                    "1500000000000000000",
                    true,
                );
                s.chain_id = 8453;
                s.not_after_unix = NOW + 300;
                s
            },
            {
                let mut s = summary(
                    "11111111-1111-1111-1111-111111111111",
                    "0x1fA9c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9c4D2",
                    "420000000000000000",
                    false,
                );
                s.not_after_unix = NOW + 300;
                s
            },
        ])));
        model.update(Msg::View(crate::app::View::Queue));
    }

    /// The queue is a table, and a table names its columns (mockup state 3).
    #[test]
    fn the_queue_names_its_columns() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let screen = draw_rows(&model, 100, 24).join("\n");
        for column in ["KIND", "AMOUNT", "RECIPIENT", "NETWORK"] {
            assert!(
                screen.contains(column),
                "column {column} missing:\n{screen}"
            );
        }
    }

    /// The approve gate's boundary, pinned from BOTH sides in one test — the
    /// device `the_positions_budget_sits_exactly_on_its_boundary` already uses
    /// for the positions panel.
    ///
    /// **Red-first is impossible here by construction:** the code is already
    /// correct, so there is no state in which this test fails before a fix —
    /// there is no fix. The falsifiability proof is the mutation, shown in the
    /// report: `<= height` → `<= height + 1` breaks the tight side, and
    /// `<= height` → `< height` breaks the exact-fit side.
    ///
    /// This is the boundary a stray `+ 1` walked past 243 green tests.
    #[test]
    fn the_approve_gate_sits_exactly_on_its_boundary() {
        const W: u16 = 100;
        let mut m = Model::default();
        to_watching(
            &mut m,
            vec![summary(
                "00000000-0000-0000-0000-000000000000",
                "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                "1000000000000000000",
                false,
            )],
        );
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card(
            "00000000-0000-0000-0000-000000000000",
            NOW + 300,
            false,
        )))));
        let policy = m.policy();
        let from = m.wallet_address();
        let Phase::Watching {
            confirm: Some(c), ..
        } = m.phase()
        else {
            panic!("a card is open");
        };

        // Found by construction, not by hand arithmetic: the test survives a
        // layout change and still pins the edge it is about.
        let tight = (6u16..80)
            .find(|h| priority_fields_fit(c, from, policy, W, *h))
            .expect("some height arms approve");
        assert!(
            !priority_fields_fit(c, from, policy, W, tight - 1),
            "one row less than the exact fit must NOT arm approve (height {tight})"
        );

        // And what is drawn agrees with the gate on both sides.
        let at_fit = draw_rows(&m, W, tight).join("\n");
        assert!(
            !at_fit.contains("TERMINAL TOO SMALL"),
            "an exact fit shows the card, no banner:\n{at_fit}"
        );
        let one_short = draw_rows(&m, W, tight - 1).join("\n");
        assert!(
            one_short.contains("TERMINAL TOO SMALL"),
            "one row short must say so and disable approve:\n{one_short}"
        );
    }

    /// The column the mockup called AGE shows time LEFT instead — the wire
    /// carries no creation time, and what matters for triage is how long the
    /// human has, not how long it has sat (Reviewer, round 10).
    #[test]
    fn the_queue_shows_how_long_is_left_not_how_long_it_sat() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains("EXPIRES"), "the column is named:\n{screen}");
        assert!(
            screen.contains("4 min") || screen.contains("5 min"),
            "and it counts down:\n{screen}"
        );
    }

    /// A deadline already past reads as expired, never as a huge number: the
    /// countdown saturates rather than wrapping (same rule as the card's).
    #[test]
    fn a_passed_deadline_reads_as_expired() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let mut s = summary(
            "00000000-0000-0000-0000-000000000000",
            "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
            "1000000000000000000",
            false,
        );
        s.not_after_unix = NOW - 10;
        model.update(Msg::Reply(Reply::List(vec![s])));
        model.update(Msg::View(crate::app::View::Queue));
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains("expired"), "{screen}");
    }

    /// Ф-4: the balance panel counted its budget and threw it away
    /// (`let _ = height;`), so content past the panel vanished with no marker —
    /// unlike positions right below it, which says what it hid.
    ///
    /// And the staleness warning is pushed last, so it was the FIRST thing to
    /// disappear. It is not data, it is the line that says the data may be
    /// wrong; it outranks a balance row and survives the truncation.
    #[test]
    fn the_balance_panel_says_what_it_hid_and_keeps_the_warning() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances: (0..10)
                    .map(|i| crate::protocol::ChainBalance {
                        chain_id: i,
                        symbol: "ETH".to_owned(),
                        balance: "1000000000000000000".to_owned(),
                        decimals: 18,
                        balance_formatted: "1".to_owned(),
                        token_address: String::new(),
                    })
                    .collect(),
                unavailable: vec![],
                allowed_chains: vec![1],
                policy: policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
            },
        )))));
        // A refresh that failed after a good one: the wallet is kept, the data
        // is flagged as possibly stale.
        model.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        model.update(Msg::View(crate::app::View::Dashboard));

        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(
            // Ten entries and the staleness note against a ceiling of eight:
            // six rows are drawn, the marker takes the seventh, four are hidden.
            // The cause is the panel's own ceiling — this window has rows to
            // spare, so blaming its size would send the human to resize for
            // nothing (round-6 MINOR-4).
            screen.contains("+4 more — panel is full"),
            "the panel must say what it hid:\n{screen}"
        );
        assert!(
            screen.contains("may be stale"),
            "and the warning must outlive the rows it warns about:\n{screen}"
        );
    }

    /// Б-3: the list must follow the cursor. Kimi walked 39 items down and the
    /// selection bar left the screen entirely — the human is deciding on a
    /// payment they cannot see. The queue is where duplicates pile up when
    /// autonomy is unconfirmed, which this wave documents as expected, so a
    /// long queue is not a corner case here.
    #[test]
    fn the_queue_follows_the_cursor_and_says_what_is_hidden() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let items: Vec<_> = (0..40)
            .map(|i| {
                let mut s = summary(
                    &format!("{i:08}-0000-0000-0000-000000000000"),
                    "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                    "1000000000000000000",
                    false,
                );
                s.not_after_unix = NOW + 300;
                s
            })
            .collect();
        model.update(Msg::Reply(Reply::List(items)));
        model.update(Msg::View(crate::app::View::Queue));

        let top = draw_rows(&model, 100, 24).join("\n");
        assert!(top.contains('▌'), "the bar is on screen at the top:\n{top}");
        assert!(
            top.contains("more — terminal too small"),
            "and what is hidden is stated, as positions and activity do:\n{top}"
        );

        for _ in 0..39 {
            model.update(Msg::MoveDown);
        }
        let bottom = draw_rows(&model, 100, 24).join("\n");
        assert!(
            bottom.contains('▌'),
            "the bar must still be on screen at the far end:\n{bottom}"
        );
    }

    /// И-1: with a card open the queue collapses to one row, and that row is
    /// the selected item — the property the collapsed strip exists for. The
    /// column header cannot also fit there, and between a heading and the item
    /// it heads, the item wins.
    #[test]
    fn the_collapsed_strip_shows_the_item_not_the_heading() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::Reply(Reply::List(vec![summary(
            "00000000-0000-0000-0000-000000000000",
            "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
            "1000000000000000000",
            false,
        )])));
        model.update(Msg::View(crate::app::View::Queue));
        model.update(Msg::Open);
        model.update(Msg::Reply(Reply::Get(GetOutcome::Card(card(
            "00000000-0000-0000-0000-000000000000",
            NOW + 300,
            false,
        )))));
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(
            screen.contains("0x8b3E"),
            "the collapsed strip shows the selected item:\n{screen}"
        );
    }

    /// The header and the rows are one table or they are not a table.
    ///
    /// `the_queue_names_its_columns` only asks whether the words are present,
    /// which is true of any two independently hand-built strings — and they
    /// were two, and they drifted: `RECIPIENT` sat over the arrow, two columns
    /// left of the address it names. This asserts the column INDEX, so a width
    /// changed in one place and not the other fails here.
    #[test]
    fn the_queue_header_sits_over_the_columns_it_names() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let rows = draw_rows(&model, 100, 24);
        let header = rows
            .iter()
            .find(|r| r.contains("RECIPIENT"))
            .expect("the header row");
        let data = rows
            .iter()
            .find(|r| r.contains("0x8b3E"))
            .expect("a data row");

        // Character offsets, not byte offsets: `▌`, `●` and `→` are multi-byte,
        // so `find` alone would compare two different rulers.
        let col = |s: &str, needle: &str| s.find(needle).map(|b| s[..b].chars().count());
        for (word, cell) in [
            ("RECIPIENT", "0x8b3E"),
            ("NETWORK", "Base"),
            // The cell CONTENT, not a fragment of it: "min" also matches
            // inside "5 min" two columns in, and would compare cell starts
            // against a position that is not one.
            ("EXPIRES", "5 min"),
        ] {
            assert_eq!(
                col(header, word),
                col(data, cell),
                "column {word} must start where {cell} starts\nheader: {header}\nrow:    {data}"
            );
        }
    }

    /// Two mechanisms shorten a queue cell — `short_eth` cuts the number,
    /// `clamp_cell` cuts the cell — and the second can land on the marker the
    /// first left behind. One `…` is the whole message; two are noise.
    #[test]
    fn clamp_cell_never_doubles_the_ellipsis() {
        assert_eq!(
            clamp_cell("0.005499… ETH", Q_AMOUNT_CELL),
            "0.005499… ETH",
            "a cell that fits is untouched, marker and all"
        );
        // A cut past the marker replaces the tail, and fills the cell exactly:
        // `Q_AMOUNT_CELL - 1` kept characters plus the one that says "there was
        // more" — asserted against the constant, not against a hand count.
        let cut = clamp_cell("120,000,000.123456… ETH", Q_AMOUNT_CELL);
        assert_eq!(cut.chars().count(), Q_AMOUNT_CELL, "fills the cell: {cut}");
        assert_eq!(cut, "120,000,000.12345…");
        assert_eq!(
            clamp_cell("12345…89", 7),
            "12345…",
            "a cut landing ON the marker keeps one, not two"
        );
    }

    /// The same statement, under an amount that does not fit its column. The
    /// fixture above carries a short `1.5 ETH`, so it proved the headings sit
    /// over the columns only for amounts we happened to pick — the live run
    /// showed a real one (`0.00549906802239073 ETH`) shoving the recipient,
    /// the network and the deadline to the right, out from under their names.
    #[test]
    fn the_queue_columns_hold_under_a_hostile_amount() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::Reply(Reply::List(vec![{
            // U256::MAX wei — 60 whole ether digits. No wallet holds it; the
            // point is that the table cannot be pushed apart by its content,
            // not that this content is plausible.
            let mut s = summary(
                "22222222-2222-2222-2222-222222222222",
                "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                "115792089237316195423570985008687907853269984665640564039457584007913129639935",
                false,
            );
            s.chain_id = 8453;
            s.not_after_unix = NOW + 300;
            s
        }])));
        model.update(Msg::View(crate::app::View::Queue));
        let rows = draw_rows(&model, 100, 24);
        let header = rows
            .iter()
            .find(|r| r.contains("RECIPIENT"))
            .expect("the header row");
        // Found by its marker, not by the recipient: an overflowing amount can
        // push the recipient off the screen entirely, and a test that looks for
        // the recipient would then report "no data row" instead of the defect.
        let data = rows.iter().find(|r| r.contains('●')).expect("a data row");

        for cell in ["0x8b3E", "Base", "5 min"] {
            assert!(
                data.contains(cell),
                "the amount must not push {cell} out of the row\nheader: {header}\nrow:    {data}"
            );
        }
        let col = |s: &str, needle: &str| s.find(needle).map(|b| s[..b].chars().count());
        for (word, cell) in [
            ("RECIPIENT", "0x8b3E"),
            ("NETWORK", "Base"),
            ("EXPIRES", "5 min"),
        ] {
            assert_eq!(
                col(header, word),
                col(data, cell),
                "column {word} must start where {cell} starts even when the amount \
                 overflows\nheader: {header}\nrow:    {data}"
            );
        }
    }

    /// Design §4 and §5: the amount is a column read down its RIGHT edge, so
    /// two amounts of different length end at the same place — and the heading
    /// ends there too.
    #[test]
    fn the_amount_column_is_right_aligned() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let rows = draw_rows(&model, 100, 24);
        let ends_at = |row_needle: &str, cell: &str| {
            let row = rows
                .iter()
                .find(|r| r.contains(row_needle))
                .unwrap_or_else(|| panic!("a row containing {row_needle}"));
            let at = row.find(cell).unwrap_or_else(|| panic!("{cell} in {row}"));
            row[..at + cell.len()].chars().count()
        };
        // `1.5 ETH` and `0.42 ETH` differ in length; right-aligned, they stop
        // at the same column.
        assert_eq!(
            ends_at("0x8b3E", "ETH"),
            ends_at("0x1fA9", "ETH"),
            "two amounts of different length must end at the same column\n{rows:#?}"
        );
        assert_eq!(
            ends_at("AMOUNT", "AMOUNT"),
            ends_at("0x8b3E", "ETH"),
            "and the heading's right edge sits over the column's\n{rows:#?}"
        );
    }

    /// Danger reads before the card is opened: a high-risk row carries `◆` and
    /// the alarm colour, an ordinary one carries `●`.
    #[test]
    fn risk_is_visible_in_the_list_itself() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains('◆'), "high-risk marker:\n{screen}");
        assert!(screen.contains('●'), "ordinary marker:\n{screen}");
        let fgs = row_fgs_containing(&model, 100, 24, "◆");
        assert!(fgs.contains(&theme::high_risk()), "and it is amber");
    }

    /// Design §5: the selected row is a left bar plus bold — **not** inversion,
    /// which fights the light theme.
    #[test]
    fn the_selected_row_is_barred_and_bold_never_inverted() {
        let mut model = Model::default();
        queued(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains('▌'), "the selection bar:\n{screen}");
        let mods = row_mods_containing(&model, 100, 24, "▌");
        assert!(mods.contains(Modifier::BOLD), "the selected row is bold");
        assert!(
            !mods.contains(Modifier::REVERSED),
            "and never inverted — inversion fights the light theme"
        );
    }

    /// §4: the card says why this payment is waiting, and the answer follows
    /// from the pair (mode, origin) — the wire carries no reason field, and
    /// within one wallet the reason is the same for every parked item.
    #[test]
    fn the_card_says_why_the_payment_is_waiting() {
        for (mode, origin, reason) in [
            (
                PolicyMode::Supervised,
                PolicyOrigin::Provisioned,
                "waiting for your decision",
            ),
            (
                PolicyMode::Autonomous,
                PolicyOrigin::Provisioned,
                "parked: autonomy unconfirmed — confirm on the Dashboard",
            ),
            (
                PolicyMode::Autonomous,
                PolicyOrigin::Acknowledged,
                "parked until mode confirmation — decided by you",
            ),
        ] {
            let mut model = Model::default();
            to_watching_with_policy(&mut model, policy_of(mode, origin));
            model.update(Msg::Reply(Reply::List(vec![summary(
                "00000000-0000-0000-0000-000000000000",
                "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                "1000000000000000000",
                false,
            )])));
            model.update(Msg::View(crate::app::View::Queue));
            model.update(Msg::Open);
            model.update(Msg::Reply(Reply::Get(GetOutcome::Card(card(
                "00000000-0000-0000-0000-000000000000",
                NOW + 300,
                false,
            )))));
            let screen = draw_rows(&model, 100, 24).join("\n");
            assert!(
                screen.contains(reason),
                "{mode:?}/{origin:?} must say why:\n{screen}"
            );
        }
    }

    /// Only the state that asks something of the human is alarm-coloured —
    /// the same rule the header follows, applied to the same fact.
    #[test]
    fn only_the_unconfirmed_reason_is_alarm_coloured() {
        for (mode, origin, needle, expect_alarm) in [
            (
                PolicyMode::Autonomous,
                PolicyOrigin::Provisioned,
                "confirm on the Dashboard",
                true,
            ),
            (
                PolicyMode::Supervised,
                PolicyOrigin::Provisioned,
                "waiting for your decision",
                false,
            ),
        ] {
            let mut model = Model::default();
            to_watching_with_policy(&mut model, policy_of(mode, origin));
            model.update(Msg::Reply(Reply::List(vec![summary(
                "00000000-0000-0000-0000-000000000000",
                "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
                "1000000000000000000",
                false,
            )])));
            model.update(Msg::View(crate::app::View::Queue));
            model.update(Msg::Open);
            model.update(Msg::Reply(Reply::Get(GetOutcome::Card(card(
                "00000000-0000-0000-0000-000000000000",
                NOW + 300,
                false,
            )))));
            let fgs = row_fgs_containing(&model, 100, 24, needle);
            assert_eq!(
                fgs.contains(&theme::high_risk()),
                expect_alarm,
                "{mode:?}/{origin:?}: alarm expected {expect_alarm}"
            );
        }
    }

    /// The identity panel (mockup, dashboard states 1–2): who this wallet is,
    /// stated in three lines on the left. The version is the crate's own — В-3
    /// ratified that the binary shows the number it can actually vouch for,
    /// not the edition number it has no honest source for.
    #[test]
    fn the_dashboard_states_who_this_wallet_is() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::View(crate::app::View::Dashboard));
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains("RUSTOK WALLET"), "the name:\n{screen}");
        assert!(
            screen.contains(&format!("console v{}", env!("CARGO_PKG_VERSION"))),
            "the version it can vouch for:\n{screen}"
        );
        assert!(
            screen.contains(&crate::format::short_addr(WALLET)),
            "and which wallet this is:\n{screen}"
        );
    }

    /// Draws the identity panel alone, with the image's claims passed in rather
    /// than read from the environment — the populated shape has to be provable
    /// without a parallel test binary mutating process-wide state.
    fn identity_rows(address: Option<&str>, versions: Versions<'_>, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_identity(f, address, versions, f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// Inside the wallet image the panel names all three layers, in the order a
    /// person asks about them: the thing they installed, the screen they are
    /// looking at, the engine underneath. The numbers line up so a changed one
    /// is seen rather than read for.
    #[test]
    fn the_identity_panel_names_every_layer_the_image_states() {
        let rows = identity_rows(
            Some(WALLET),
            Versions {
                wallet: Some("0.9.3"),
                core: Some("0.4.1"),
            },
            IDENTITY_COL,
            8,
        );
        let screen = rows.join("\n");
        assert!(
            screen.contains("wallet   v0.9.3"),
            "what the human installed:\n{screen}"
        );
        assert!(
            screen.contains(&format!("console  v{}", env!("CARGO_PKG_VERSION"))),
            "the screen they are looking at:\n{screen}"
        );
        assert!(
            screen.contains("core     v0.4.1"),
            "the engine underneath:\n{screen}"
        );
        let wallet_row = rows.iter().position(|r| r.contains("wallet")).unwrap();
        let console_row = rows.iter().position(|r| r.contains("console")).unwrap();
        let core_row = rows.iter().position(|r| r.contains("core")).unwrap();
        assert!(
            wallet_row < console_row && console_row < core_row,
            "outermost first, engine last:\n{screen}"
        );
    }

    /// Outside the image — a bare `cargo run`, or the console's own container —
    /// the panel says exactly what it said before this circle: one line, for
    /// the binary that knows itself. Silence, not the word `unknown`: absence
    /// is not a value, and a word standing where a number belongs invites the
    /// reader to treat it as one.
    #[test]
    fn outside_the_wallet_image_the_panel_says_only_what_the_binary_knows() {
        let rows = identity_rows(Some(WALLET), Versions::default(), IDENTITY_COL, 8);
        let screen = rows.join("\n");
        assert!(
            screen.contains(&format!("console v{}", env!("CARGO_PKG_VERSION"))),
            "the one number it can vouch for, in the shape it always had:\n{screen}"
        );
        assert!(
            !screen.contains("unknown") && !screen.contains('?'),
            "no placeholder standing in for a number:\n{screen}"
        );
        assert!(
            !screen.contains("core"),
            "no row for a layer that said nothing:\n{screen}"
        );
    }

    /// One source missing does not silence the other. The image can state a
    /// wallet version without a core pin — a hand-built image, a future layout
    /// — and the row that has an answer still shows it.
    #[test]
    fn a_layer_that_states_nothing_leaves_no_row_and_takes_none_with_it() {
        let rows = identity_rows(
            Some(WALLET),
            Versions {
                wallet: Some("0.9.3"),
                core: None,
            },
            IDENTITY_COL,
            8,
        );
        let screen = rows.join("\n");
        assert!(
            screen.contains("wallet   v0.9.3"),
            "the stated one:\n{screen}"
        );
        assert!(
            screen.contains(&format!("console  v{}", env!("CARGO_PKG_VERSION"))),
            "and the one that knows itself:\n{screen}"
        );
        assert!(
            !screen.contains("core"),
            "but nothing for the silent one:\n{screen}"
        );
    }

    /// What the image says is read at a boundary, so it is judged there. Blank
    /// is not a version; a control character would tear the frame this panel is
    /// drawn inside. Both read as absent rather than as a value — and the `v`
    /// the two sources disagree about is normalized away, so the panel states
    /// one shape whichever side of the build a number came from.
    #[test]
    fn a_blank_or_torn_version_reads_as_absent() {
        assert_eq!(parse_stated_version("0.9.3").as_deref(), Some("0.9.3"));
        assert_eq!(parse_stated_version("v0.4.1").as_deref(), Some("0.4.1"));
        assert_eq!(parse_stated_version("  0.9.3  ").as_deref(), Some("0.9.3"));
        assert_eq!(parse_stated_version(""), None);
        assert_eq!(parse_stated_version("   "), None);
        assert_eq!(parse_stated_version("0.9.3\nRUSTOK WALLET"), None);
        assert_eq!(parse_stated_version("0.9.3\u{1b}[31m"), None);
        // A bare `v` is a label with no number behind it. The judgement has to
        // come AFTER the `v` is taken off, or this reads as an empty version
        // and the panel draws `wallet   v`.
        assert_eq!(parse_stated_version("v"), None);
        assert_eq!(parse_stated_version("  v  "), None);
        // A bidirectional override is not a control character and passes that
        // weaker test — while reordering the glyphs on the one panel whose job
        // is to say truthfully what is running.
        assert_eq!(parse_stated_version("0.9.3\u{202e}"), None);
        assert_eq!(parse_stated_version("0.9.\u{0663}"), None);
    }

    /// The column is narrow and a version is not obliged to be short. Every
    /// other overflow in this file ends in an explicit marker rather than a
    /// silent clip, and this row is no exception — the fence is the same
    /// `clamp_cell` the queue cells use.
    #[test]
    fn a_version_too_long_for_the_column_is_cut_with_a_marker() {
        const CELL: usize = IDENTITY_COL as usize - 2;
        let lines = version_lines(Versions {
            wallet: Some("0.9.3-rc.1+build.20260809"),
            core: None,
        });
        let wallet = lines.iter().find(|l| l.starts_with("wallet")).unwrap();
        assert_eq!(
            wallet.chars().count(),
            CELL,
            "it fills the cell and no more: {wallet}"
        );
        assert!(
            wallet.ends_with('…'),
            "and says it was cut, rather than clipping in silence: {wallet}"
        );
    }

    /// The panel is **wired**, not merely shaped: what the image states reaches
    /// the screen through the whole render chain, not just through the function
    /// that formats the rows.
    ///
    /// This is the test that was missing. The versions used to be read inside
    /// the dashboard, from an environment the test binary deliberately kept
    /// empty — so dropping them anywhere along the way left every test green,
    /// and only an external script noticed. Passing them in from the top costs
    /// one argument and makes that regression ordinary to catch.
    #[test]
    fn what_the_image_states_reaches_the_screen_through_the_whole_chain() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::View(crate::app::View::Dashboard));
        let screen = draw_rows_with(
            &model,
            100,
            24,
            NOW,
            Versions {
                wallet: Some("0.9.3"),
                core: Some("0.4.1"),
            },
        )
        .join("\n");
        assert!(
            screen.contains("wallet   v0.9.3"),
            "the wallet's number reached the dashboard:\n{screen}"
        );
        assert!(
            screen.contains("core     v0.4.1"),
            "and so did the core's:\n{screen}"
        );
    }

    /// The fourth combination: the core states its version and the wallet does
    /// not. Neither row depends on the other having an answer.
    #[test]
    fn the_core_alone_still_gets_its_row() {
        let rows = identity_rows(
            Some(WALLET),
            Versions {
                wallet: None,
                core: Some("0.4.1"),
            },
            IDENTITY_COL,
            8,
        );
        let screen = rows.join("\n");
        assert!(
            screen.contains("core     v0.4.1"),
            "the stated one:\n{screen}"
        );
        assert!(
            screen.contains(&format!("console  v{}", env!("CARGO_PKG_VERSION"))),
            "and the one that knows itself:\n{screen}"
        );
        assert!(
            !screen.contains("wallet"),
            "but nothing for the silent one:\n{screen}"
        );
    }

    /// The panels carry the titles the mockup names, so a human reading the
    /// screen and a human reading the design see the same words.
    #[test]
    fn the_dashboard_content_sits_in_named_panels() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Supervised, PolicyOrigin::Provisioned),
        );
        model.update(Msg::View(crate::app::View::Dashboard));
        let rows = draw_rows(&model, 100, 24);
        for title in ["Queue", "balance", "positions"] {
            // A framed title, not the bare word: "Queue" is also a tab and
            // "balance" was already a flat label, so a substring check passes
            // before the panels exist and proves nothing.
            assert!(
                rows.iter()
                    .any(|r| r.contains(title) && r.contains('┌') && r.contains('─')),
                "panel {title} is not a titled frame:\n{}",
                rows.join("\n")
            );
        }
    }

    /// Mockup state 2: while the mode is unconfirmed the queue block says why
    /// the queue is a queue at all, instead of repeating the count as if this
    /// were an ordinary backlog.
    #[test]
    fn the_queue_panel_says_why_everything_is_parked_when_unconfirmed() {
        const WHY: &str = "all parked, mode unconfirmed";
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Autonomous, PolicyOrigin::Provisioned),
        );
        model.update(Msg::Reply(Reply::List(vec![summary(
            "00000000-0000-0000-0000-000000000000",
            "0x8b3E4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c91Aa",
            "1000000000000000000",
            false,
        )])));
        model.update(Msg::View(crate::app::View::Dashboard));
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains(WHY), "{screen}");
    }

    /// The banner exists only where there is something to confirm. A permanent
    /// strip for a once-in-a-wallet action is the same standing reserve the
    /// card just gave up (design §3).
    #[test]
    fn the_dashboard_offers_confirmation_only_when_there_is_something_to_confirm() {
        const TITLE: &str = "Autonomous mode unconfirmed";
        for (mode, origin, expected) in [
            (PolicyMode::Autonomous, PolicyOrigin::Provisioned, true),
            (PolicyMode::Autonomous, PolicyOrigin::Acknowledged, false),
            (PolicyMode::Supervised, PolicyOrigin::Provisioned, false),
            (PolicyMode::ReadOnly, PolicyOrigin::Provisioned, false),
            (PolicyMode::Unknown, PolicyOrigin::Provisioned, false),
        ] {
            let mut model = Model::default();
            to_watching_with_policy(&mut model, policy_of(mode, origin));
            model.update(Msg::View(crate::app::View::Dashboard));
            let screen = draw_rows(&model, 100, 24).join("\n");
            assert_eq!(
                screen.contains(TITLE),
                expected,
                "{mode:?}/{origin:?}: banner expected {expected}\n{screen}"
            );
        }
    }

    /// Transcribed from design §3 — a refusal the human cannot act on is a dead
    /// end, so the banner states what is happening and which key ends it.
    #[test]
    fn the_confirmation_banner_says_what_happens_and_what_to_press() {
        const WHAT_HAPPENS: &str = "Every send queues and waits for you.";
        const WHAT_TO_PRESS: &str = "[c] — confirm autonomy";
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Autonomous, PolicyOrigin::Provisioned),
        );
        model.update(Msg::View(crate::app::View::Dashboard));
        let screen = draw_rows(&model, 100, 24).join("\n");
        assert!(screen.contains(WHAT_HAPPENS), "what happens:\n{screen}");
        assert!(screen.contains(WHAT_TO_PRESS), "what to press:\n{screen}");
        assert!(
            screen.contains("requires PIN"),
            "and that it will ask for the PIN:\n{screen}"
        );
    }

    /// The banner is the one framed thing on the Dashboard, and it carries the
    /// alarm colour — the same signal the header spends on this one state.
    #[test]
    fn the_confirmation_banner_is_alarm_coloured() {
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Autonomous, PolicyOrigin::Provisioned),
        );
        model.update(Msg::View(crate::app::View::Dashboard));
        let fgs = row_fgs_containing(&model, 100, 24, "Autonomous mode unconfirmed");
        assert!(
            fgs.contains(&theme::high_risk()),
            "the banner must read as the thing that wants attention"
        );
    }

    /// The ratified phrase, transcribed from the design decision (§2 table) and
    /// not from the code: this is the one string in the slice that carries the
    /// alarm colour, so it earns literal accuracy rather than a paraphrase.
    ///
    /// Wide terminals are where it renders — the tabs take 59 columns, so the
    /// full wording needs 105. Narrower ones get the approved short form,
    /// which is a different question from whether the designed phrase exists at
    /// all.
    #[test]
    fn the_designed_alarm_phrase_renders_verbatim_when_it_fits() {
        const DESIGNED: &str = "autonomous · unconfirmed — sends wait for you";
        let mut model = Model::default();
        to_watching_with_policy(
            &mut model,
            policy_of(PolicyMode::Autonomous, PolicyOrigin::Provisioned),
        );
        model.update(Msg::Resize {
            width: 120,
            height: 24,
        });
        let header = draw_rows(&model, 120, 24)[0].clone();
        assert!(
            header.contains(DESIGNED),
            "the ratified wording must be reachable, not only a paraphrase of it:\n{header}"
        );
    }

    /// Before the first `context` reply the console has not been told the mode.
    /// It says nothing rather than inventing a placeholder — an invented one is
    /// a claim about whether this wallet spends by itself.
    #[test]
    fn an_unknown_mode_states_nothing() {
        let mut model = Model::default();
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
        let header = draw_rows(&model, 80, 24)[0].clone();
        for word in ["confirmed", "manual", "read-only", "autonomous"] {
            assert!(
                !header.contains(word),
                "nothing is known yet, so nothing is claimed: {header}"
            );
        }
    }

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
                unavailable: vec![],
                allowed_chains: vec![1],
                policy: Default::default(),
            },
        )))));
        model.update(Msg::View(crate::app::View::Queue)); // Stage-5 home is Dashboard
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
        model.update(Msg::View(crate::app::View::Queue)); // Stage-5 home is Dashboard
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
        // What this pins is the PAIRING: the recipient and the amount of the
        // SAME item on one line, so a swap would split them across lines.
        // The representation moved with the design — the queue is a reading
        // list and shortens the address (as Activity already does), while the
        // card stays the signing surface and shows it in full. The property
        // did not move.
        assert!(
            has_line_with(
                &rows,
                &[
                    &crate::format::short_addr("0x742d35Cc6634C0532925a3b844Bc454e4438f44e"),
                    "0.1 ETH",
                ],
            ),
            "recipient and amount of one item must render together"
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

    /// The card names its network the way every other screen names it. It used
    /// to print the bare id, so one wallet called one network two different
    /// things depending on which screen you were looking at.
    #[test]
    fn the_card_names_the_network_it_signs_on() {
        let mut m = Model::new();
        open_card(&mut m, "a1", NOW + 27, false);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["network", "Ethereum"]),
            "the card names the network:\n{rows:#?}"
        );
    }

    /// …and the card is the one surface that must NOT shorten the amount: it is
    /// where the human decides how much leaves the wallet. The queue's
    /// shortening would hide digits exactly where they are being approved —
    /// the same boundary `short_addr` keeps for addresses.
    #[test]
    fn the_card_keeps_the_exact_amount() {
        const EXACT: &str = "0.00549906802239073 ETH";
        let mut m = Model::new();
        to_watching(
            &mut m,
            vec![summary("a1", "0xabc", "5499068022390730", false)],
        );
        m.update(Msg::Open);
        let mut c = card("a1", NOW + 27, false);
        c.amount_wei = "5499068022390730".to_owned();
        m.update(Msg::Reply(Reply::Get(crate::protocol::GetOutcome::Card(c))));

        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["amount", EXACT]),
            "the card shows every digit:\n{rows:#?}"
        );
        let amount_line = rows
            .iter()
            .find(|r| r.contains("amount"))
            .expect("the amount line");
        assert!(
            !amount_line.contains('…'),
            "and marks nothing as dropped: {amount_line}"
        );
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
        terminal
            .draw(|f| render(f, model, NOW, Versions::default()))
            .unwrap();
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
        terminal
            .draw(|f| render(f, &m, NOW, Versions::default()))
            .unwrap();
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

    // ── Stage 5: the Dashboard view ──

    use crate::protocol::{AssetUnavailable, ChainBalance, Position, PositionsOutcome};

    /// Drive a model onto the (home) Dashboard with the given balances, then
    /// feed it the positions reply the scheduler solicits.
    fn to_dashboard(balances: Vec<ChainBalance>, positions: PositionsOutcome) -> Model {
        to_dashboard_with_unavailable(balances, Vec::new(), positions)
    }

    /// Same, with assets the core could not read at all.
    fn to_dashboard_with_unavailable(
        balances: Vec<ChainBalance>,
        unavailable: Vec<AssetUnavailable>,
        positions: PositionsOutcome,
    ) -> Model {
        let mut m = to_dashboard_loading(balances, unavailable);
        m.update(Msg::Reply(Reply::Positions(positions)));
        m
    }

    /// A native ETH row as the core sends it: raw wei, the 18 places they are
    /// counted in, and the string the core already rendered from the two.
    fn native_row(chain_id: u64, wei: &str, formatted: &str) -> ChainBalance {
        ChainBalance {
            chain_id,
            symbol: "ETH".to_owned(),
            balance: wei.to_owned(),
            decimals: 18,
            balance_formatted: formatted.to_owned(),
            token_address: String::new(),
        }
    }

    /// Same, stopped BEFORE the positions reply lands (the loading state).
    fn to_dashboard_loading(
        balances: Vec<ChainBalance>,
        unavailable: Vec<AssetUnavailable>,
    ) -> Model {
        let mut m = Model::new();
        m.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        m.update(Msg::PinDigit('1'));
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances,
                unavailable,
                allowed_chains: vec![1],
                policy: Default::default(),
            },
        )))));
        m.update(Msg::Tick);
        // The scheduler answers this list reply with the positions request.
        m.update(Msg::Reply(Reply::List(vec![])));
        m
    }

    fn aave_position() -> Position {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("health_factor".to_owned(), "∞".to_owned());
        extra.insert("ltv".to_owned(), "80%".to_owned());
        Position {
            protocol: "aave_v3".to_owned(),
            chain_id: 1,
            asset_address: "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2".to_owned(),
            asset_symbol: "USD".to_owned(),
            asset_name: "Aave v3 account".to_owned(),
            asset_decimals: 8,
            balance: "100000000000".to_owned(),
            balance_formatted: "1000".to_owned(),
            extra,
        }
    }

    /// The balance panel is a scan surface too: the live run showed seventeen
    /// fractional digits on the first screen after unlock. The exact figure is
    /// not lost — it is on the card, where a decision is made.
    #[test]
    fn the_balance_panel_shortens_a_long_amount() {
        let balances = vec![native_row(1, "5499068022390730", "0.00549906802239073")];
        let m = to_dashboard(balances, PositionsOutcome::Ok(vec![aave_position()]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Ethereum", "0.005499… ETH"]),
            "the balance is shortened for scanning:\n{rows:#?}"
        );
    }

    /// A registry token row — the live USDC on Arbitrum unless told otherwise.
    fn token_row(chain_id: u64, symbol: &str, raw: &str, formatted: &str) -> ChainBalance {
        ChainBalance {
            chain_id,
            symbol: symbol.to_owned(),
            balance: raw.to_owned(),
            decimals: 6,
            balance_formatted: formatted.to_owned(),
            token_address: "0xaf88d065e77c8cC2239327C5EDb3A432268e5831".to_owned(),
        }
    }

    fn unread(chain_id: u64, symbol: &str, reason: &str) -> AssetUnavailable {
        AssetUnavailable {
            chain_id,
            symbol: symbol.to_owned(),
            reason: reason.to_owned(),
            token_address: String::new(),
        }
    }

    /// Test 10 (spec §S2). The row this whole arc exists for, on the screen: the
    /// symbol the operator registered and the amount the core rendered at the
    /// token's own six places.
    ///
    /// Both halves are needed, and the positive one carries more weight than its
    /// first draft claimed. Re-basing 22820562 raw units at ether's eighteen
    /// places gives `0.000000000022820562` — but the dust floor would render
    /// that as `<0.000001 ETH`, so the literal string below would not appear
    /// either way. What actually catches a return to `short_eth(&b.balance)` is
    /// the assertion that `22.820562 USDC` IS on the screen; the negative one
    /// guards the narrower case of an un-floored eighteen-place render.
    #[test]
    fn the_balance_panel_prints_a_token_in_its_own_unit() {
        let balances = vec![
            native_row(42161, "6700000000000000", "0.0067"),
            token_row(42161, "USDC", "22820562", "22.820562"),
        ];
        let m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Arbitrum", "22.820562 USDC"]),
            "the token reads in its own unit:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("0.000000000022820562")),
            "a token re-based at ether's 18 places would read as dust:\n{rows:#?}"
        );
        // The native row of the same chain is untouched by any of this.
        assert!(has_line_with(&rows, &["Arbitrum", "0.0067 ETH"]));
    }

    /// Test 11 (spec §S2). The shape the acceptance criteria name: three chains,
    /// one token, and the staleness note — five lines that must all be on the
    /// screen at once.
    ///
    /// Red against the panel's old fixed height: four rows inside meant the
    /// first token pushed two real balances behind `+2 more`, and the USDC this
    /// arc exists for was one of them.
    #[test]
    fn a_token_and_three_chains_and_the_stale_note_all_fit() {
        let balances = vec![
            native_row(1, "10000000000000000", "0.01"),
            native_row(8453, "20000000000000000", "0.02"),
            native_row(42161, "6700000000000000", "0.0067"),
            token_row(42161, "USDC", "22820562", "22.820562"),
        ];
        let mut m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        // A refresh that failed after a good one: the rows stay, flagged stale.
        m.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        let rows = draw_rows(&m, 100, 24);
        for expected in [
            "0.01 ETH",
            "0.02 ETH",
            "0.0067 ETH",
            "22.820562 USDC",
            "may be stale",
        ] {
            assert!(
                rows.iter().any(|r| r.contains(expected)),
                "`{expected}` must be on the screen:\n{rows:#?}"
            );
        }
        assert!(
            !rows.join("\n").contains("more — terminal too small"),
            "nothing was hidden, so nothing may claim it was:\n{rows:#?}"
        );
    }

    /// Test 12 (spec §S2). The ceiling is a ceiling: past it the panel says how
    /// much it hid, and the line warning the numbers may be wrong still outranks
    /// the numbers themselves.
    #[test]
    fn past_the_ceiling_the_panel_says_what_it_hid_and_keeps_the_warning() {
        let balances: Vec<ChainBalance> = (0..12)
            .map(|i| native_row(i, "10000000000000000", "0.01"))
            .collect();
        let mut m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            // Twelve one-row entries and a staleness note: the ceiling of eight
            // leaves seven rows for entries, one of which the marker takes, so
            // six are drawn and six are named as hidden.
            rows.join("\n").contains("+6 more — panel is full"),
            "the panel must say what it hid, and why it will not show more — this \
             terminal has rows to spare, the ceiling is what stopped it:\n{rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("may be stale")),
            "and the warning must outlive the rows it warns about:\n{rows:#?}"
        );
    }

    /// The other cause of the same marker, and it must not borrow the first
    /// one's wording: here the window really is too short, and resizing it
    /// really does help.
    #[test]
    fn a_short_terminal_says_so_instead_of_blaming_the_ceiling() {
        let balances: Vec<ChainBalance> = (0..9)
            .map(|i| native_row(i, "10000000000000000", "0.01"))
            .collect();
        let m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        // Nine entries ask for the full ceiling (eight rows plus borders), but a
        // nine-row window has only five to give after the header and the queue —
        // so the panel is cut BELOW its own ceiling, and the window is why.
        let rows = draw_rows(&m, 100, 9);
        let screen = rows.join("\n");
        assert!(
            screen.contains("more — terminal too small"),
            "a short window is named as the cause:\n{rows:#?}"
        );
        assert!(
            !screen.contains("panel is full"),
            "and the ceiling is not blamed for it:\n{rows:#?}"
        );
    }

    /// Test 12-бис (spec §S2). When the panel has to cut, it cuts numbers — an
    /// unread asset stays on the screen.
    ///
    /// Red against the obvious order (balances first, warnings after): the
    /// truncation takes from the end, so warnings written last are the first to
    /// go, and a wallet that could not read USDT would look exactly like a
    /// wallet that holds none.
    #[test]
    fn truncation_hides_numbers_and_keeps_the_assets_it_could_not_read() {
        let balances: Vec<ChainBalance> = (0..10)
            .map(|i| native_row(i, "10000000000000000", "0.01"))
            .collect();
        let unavailable = vec![
            unread(8453, "ETH", "no_rpc_configured"),
            unread(42161, "USDT", "call_reverted"),
        ];
        let m = to_dashboard_with_unavailable(balances, unavailable, PositionsOutcome::Ok(vec![]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            rows.join("\n").contains("more — panel is full"),
            "this panel is over its ceiling — the test is vacuous otherwise:\n{rows:#?}"
        );
        assert!(
            has_line_with(&rows, &["Base", "ETH", "not queried — no RPC"]),
            "an unread native asset survives the cut:\n{rows:#?}"
        );
        assert!(
            has_line_with(
                &rows,
                &[
                    "Arbitrum",
                    "USDT",
                    "not read — call reverted, check the registry"
                ]
            ),
            "and so does an unread token, with the reason pinned whole — it is \
             what tells the operator the registry is wrong, not the network:\n{rows:#?}"
        );
    }

    /// MINOR-1: the contract reaches the screen, so a token is never mistaken
    /// for the chain's own coin.
    ///
    /// The symbol cannot carry that weight — the protocol says so itself: native
    /// USDC and bridged USDC.e share one. A registry entry that named a token
    /// "ETH" would otherwise draw a row identical to the native one, on the
    /// panel whose whole job is to say what is held.
    #[test]
    fn a_token_row_shows_the_contract_that_identifies_it() {
        let balances = vec![
            native_row(42161, "6700000000000000", "0.0067"),
            // A registry that calls its token ETH — a bug on the core's side, or
            // a hostile entry; either way the panel must not agree.
            token_row(42161, "ETH", "22820562", "22.820562"),
        ];
        let m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Arbitrum", "22.820562 ETH", "0xaf88d0…5831"]),
            "the token names its contract:\n{rows:#?}"
        );
        // And the native row of the same chain does NOT — an empty contract is
        // what marks it native, and printing something there would invent one.
        assert!(
            rows.iter()
                .any(|r| r.contains("0.0067 ETH") && !r.contains("0x")),
            "the native row carries no contract:\n{rows:#?}"
        );
    }

    /// MINOR-1, the other half. The symbol is the one string on this panel a
    /// human types by hand, into an operator's registry entry. It gets the same
    /// judgement the version panel makes about a version string, and for the
    /// same reason: a bidirectional override is not a control character, passes
    /// that test, and reorders the glyphs around it.
    #[test]
    fn a_symbol_that_could_tear_the_panel_is_not_drawn() {
        let mut hostile = token_row(42161, "U\u{202E}SDC", "22820562", "22.820562");
        hostile.balance_formatted = "22.820562".to_owned();
        let m = to_dashboard(vec![hostile], PositionsOutcome::Ok(vec![]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            !rows.iter().any(|r| r.contains('\u{202E}')),
            "the override never reaches the screen:\n{rows:#?}"
        );
        assert!(
            has_line_with(&rows, &["Arbitrum", "22.820562 ?"]),
            "and the amount is still shown, with the symbol stood in for:\n{rows:#?}"
        );
    }

    /// Round-6 BLOCKER, regression test. The panel budgeted one row per entry
    /// while the renderer wrapped long text onto more than one, so a single
    /// unread asset with a long reason cost two rows and was paid for with one.
    ///
    /// At 80 columns — the width every other dashboard test in this file uses —
    /// the panel is 56 cells wide inside its borders, and
    /// `  Arbitrum  USDT  not read — call reverted, check the registry` is
    /// longer than that. Three balances plus that one warning is four entries,
    /// nowhere near the ceiling of eight, and the panel still truncated: two
    /// real balances went behind `+2 more`. That is the exact defect Р7 was
    /// written to remove, arriving through a different door.
    #[test]
    fn a_wrapped_warning_does_not_push_balances_off_a_normal_terminal() {
        let balances = vec![
            native_row(1, "10000000000000000", "0.01"),
            native_row(8453, "20000000000000000", "0.02"),
            native_row(42161, "6700000000000000", "0.0067"),
        ];
        let unavailable = vec![unread(42161, "USDT", "call_reverted")];
        let m = to_dashboard_with_unavailable(balances, unavailable, PositionsOutcome::Ok(vec![]));
        let rows = draw_rows(&m, 80, 24);
        assert!(
            !rows.join("\n").contains("more — "),
            "four entries are under the ceiling of eight — nothing may be hidden:\n{rows:#?}"
        );
        for expected in ["0.01 ETH", "0.02 ETH", "0.0067 ETH"] {
            assert!(
                rows.iter().any(|r| r.contains(expected)),
                "`{expected}` must survive a wrapped warning above it:\n{rows:#?}"
            );
        }
        // And the warning that cost two rows is itself whole, both halves drawn.
        // The seam falls mid-word because wrapping counts display cells, not
        // words (`chunk_display_width`), so the tail is matched as the renderer
        // actually leaves it — pinning the real behaviour rather than a prettier
        // one this console does not have.
        assert!(
            rows.iter().any(|r| r.contains("not read — call reverted")),
            "the warning's first row:\n{rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("gistry")),
            "and its wrapped remainder — the entry was not cut in half:\n{rows:#?}"
        );
    }

    /// The other half of the blocker's cure, and a guard on my own fix: rows are
    /// grouped by asset, so truncation falls BETWEEN entries and never through
    /// one. A warning cut at the seam would leave "not read — call reverted,"
    /// on screen with the half that says what to do about it gone.
    #[test]
    fn truncation_never_leaves_half_an_entry_on_the_screen() {
        // Five two-row warnings against a ceiling of eight, so the cut falls on
        // a LATER entry — the first one is never the one at risk, which is what
        // made an earlier version of this test pass against the very mutation it
        // was written to catch.
        let unavailable: Vec<AssetUnavailable> = (0..5)
            .map(|i| unread(42161, &format!("TK{i}"), "call_reverted"))
            .collect();
        let m = to_dashboard_with_unavailable(vec![], unavailable, PositionsOutcome::Ok(vec![]));
        // 80 columns is where that reason wraps onto two rows.
        let rows = draw_rows(&m, 80, 24);
        let screen = rows.join("\n");
        assert!(
            screen.contains("more — panel is full"),
            "this panel must be truncating — the test is vacuous otherwise:\n{rows:#?}"
        );
        // Every warning that started has its remainder on the screen. Budgeting
        // one row for a two-row entry draws a fourth warning whose tail the box
        // then clips, and these two counts come apart.
        // The seam moves with the length of the symbol before it, so the tail is
        // matched by a suffix short enough to survive wherever it falls.
        let started = rows.iter().filter(|r| r.contains("not read")).count();
        let finished = rows.iter().filter(|r| r.contains("istry")).count();
        assert_eq!(
            started, finished,
            "every warning drawn is drawn whole — {started} started, {finished} \
             finished:\n{rows:#?}"
        );
    }

    /// An asset the core could not read is not an asset worth zero. The panel
    /// says which one, on which chain, and why — in words, not in the core's
    /// wire vocabulary.
    #[test]
    fn an_unread_asset_is_named_not_omitted() {
        let m = to_dashboard_with_unavailable(
            vec![native_row(1, "10000000000000000", "0.01")],
            vec![unread(8453, "ETH", "rpc_call_failed")],
            PositionsOutcome::Ok(vec![]),
        );
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Base", "ETH", "not read — RPC call failed"]),
            "the unread chain is named:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("rpc_call_failed")),
            "the wire word is for the wire:\n{rows:#?}"
        );
    }

    /// A reason this console has never heard of is quoted, not guessed at: the
    /// core may learn one before the console does.
    #[test]
    fn an_unknown_reason_is_shown_as_it_came() {
        let m = to_dashboard_with_unavailable(
            vec![],
            vec![unread(1, "ETH", "provider_quota_exhausted")],
            PositionsOutcome::Ok(vec![]),
        );
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Ethereum", "not read — provider_quota_exhausted"]),
            "an unknown reason is quoted:\n{rows:#?}"
        );
    }

    #[test]
    fn the_dashboard_shows_balance_positions_and_the_waiting_count() {
        let balances = vec![native_row(1, "10000000000000000", "0.01")];
        let m = to_dashboard(balances, PositionsOutcome::Ok(vec![aave_position()]));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            has_line_with(&rows, &["Ethereum", "0.01 ETH"]),
            "the balance reads humanly, per chain — and the chain has the name it \
             carries everywhere else in the wallet"
        );
        // The unit belongs to the amount, and the amount already carries it. The
        // panel used to append `symbol` on top, so the first screen after unlock
        // read `0.01 ETH ETH`.
        assert!(
            !rows.iter().any(|r| r.contains("ETH ETH")),
            "the unit is stated once:\n{rows:#?}"
        );
        assert!(
            has_line_with(&rows, &["aave_v3", "1000 USD", "Aave v3 account"]),
            "a position line carries protocol, formatted balance, and name"
        );
        assert!(
            has_line_with(&rows, &["health_factor ∞"]) && has_line_with(&rows, &["ltv 80%"]),
            "extra values render verbatim — display strings, never parsed"
        );
        assert!(
            has_line_with(&rows, &["Waiting for you: nothing pending"]),
            "the waiting block is present even when idle"
        );
        assert!(
            !rows.join("\n").contains(&aave_position().asset_address),
            "asset addresses are not rendered on the dashboard (Gate-1 №4)"
        );
    }

    #[test]
    fn the_dashboard_loading_state_is_not_unavailable() {
        let m = to_dashboard_loading(vec![], vec![]);
        let screen = draw(&m, 80, 24);
        assert!(screen.contains("loading positions"), "NotYet says loading");
        assert!(
            !screen.contains("positions unavailable"),
            "…and never claims unavailability it has not observed"
        );
    }

    #[test]
    fn the_dashboard_degrades_honestly() {
        // wallet_locked positions → unavailable; empty list → no positions.
        let m = to_dashboard(vec![], PositionsOutcome::WalletLocked);
        assert!(draw(&m, 80, 24).contains("positions unavailable"));

        let m = to_dashboard(vec![], PositionsOutcome::Ok(vec![]));
        let screen = draw(&m, 80, 24);
        assert!(screen.contains("no DeFi positions"));
        assert!(
            screen.contains("no balances reported"),
            "empty balances are named, not blank"
        );
    }

    #[test]
    fn a_failed_balance_refresh_is_flagged_on_the_dashboard() {
        let balances = vec![native_row(1, "5", "0.000000000000000005")];
        let mut m = to_dashboard(balances, PositionsOutcome::Ok(vec![]));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        let screen = draw(&m, 80, 24);
        assert!(
            screen.contains("may be stale"),
            "old data shown as possibly stale, never as fresh"
        );
    }

    #[test]
    fn overflowing_positions_end_with_an_explicit_marker() {
        let many: Vec<Position> = (0..40)
            .map(|i| {
                let mut p = aave_position();
                p.asset_symbol = format!("SYM{i}");
                p
            })
            .collect();
        let m = to_dashboard(vec![], PositionsOutcome::Ok(many));
        let rows = draw_rows(&m, 100, 24);
        assert!(
            rows.join("\n").contains("more — terminal too small"),
            "clipped positions say so out loud (raw_data honesty pattern)"
        );
    }

    /// Positions with distinct symbols, one render row each at width 100.
    fn many_positions(n: usize) -> Vec<Position> {
        (0..n)
            .map(|i| {
                let mut p = aave_position();
                p.asset_symbol = format!("SYM{i}");
                p.extra.clear(); // keep each row single-line at this width
                p
            })
            .collect()
    }

    fn position_rows(rows: &[String]) -> usize {
        rows.iter().filter(|r| r.contains("SYM")).count()
    }

    #[test]
    fn the_positions_budget_sits_exactly_on_its_boundary() {
        // Geometry at 100×24 after the panel split: header row (1) + Queue
        // panel (3) + balance panel leaves the rest to positions, whose own
        // borders take 2. With no balances the panel asks for one line plus its
        // borders (3), so the budget here is 15. The numbers moved with the
        // layout — twice now, and for the same reason both times: the panel
        // above stopped being a fixed height. What this pins did not move — the
        // Gate-2 blocker subtracted the header TWICE and cut positions that fit,
        // so both edges stay pinned.
        let m = to_dashboard(vec![], PositionsOutcome::Ok(many_positions(15)));
        let rows = draw_rows(&m, 100, 24);
        assert_eq!(
            position_rows(&rows),
            15,
            "an exact fit shows every position, no marker"
        );
        assert!(!rows.join("\n").contains("more — terminal too small"));

        let m = to_dashboard(vec![], PositionsOutcome::Ok(many_positions(16)));
        let rows = draw_rows(&m, 100, 24);
        assert_eq!(
            position_rows(&rows),
            14,
            "one over: the positions that fit stay, the marker takes the last \
             row — nothing that fits is hidden"
        );
        assert!(rows.join("\n").contains("more — terminal too small"));
    }

    #[test]
    fn the_waiting_block_counts_pending_items() {
        // The non-empty branch never rendered in any test (Gate-2 NIT).
        let mut m = to_dashboard(vec![], PositionsOutcome::Ok(vec![]));
        m.update(Msg::Tick);
        m.update(Msg::Reply(Reply::List(vec![
            summary("a1", "0xabc", "0", false),
            summary("a2", "0xdef", "0", false),
        ])));
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["Waiting for you: 2 pending", "press a"]),
            "the human is told how many decisions wait and how to get there"
        );
    }

    #[test]
    fn the_dashboard_tab_is_first_and_active_on_the_home_view() {
        let m = to_dashboard(vec![], PositionsOutcome::Ok(vec![]));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, &m, NOW, Versions::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..80).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(
            row.contains("Dashboard [d]")
                && row.contains("Queue·0 [a]")
                && row.contains("Receive [r]"),
            "all three tabs with their keys: {row}"
        );
        let dash_at = row.find("Dashboard").unwrap() as u16;
        assert!(
            buffer[(dash_at, 0)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "the active (home) tab is the Dashboard"
        );
    }

    // ── Stage 7: the Activity view ──

    fn history(id: &str, unix: u64, state: OutcomeState) -> HistoryEntry {
        HistoryEntry {
            unix,
            id: id.to_owned(),
            state,
            to: None,
            amount_wei: None,
            chain_id: None,
            tx_hash: None,
            reason: None,
        }
    }

    fn on_activity(entries: Vec<HistoryEntry>) -> Model {
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        m.set_history(entries);
        m.update(Msg::View(View::Activity));
        m
    }

    /// Activity is read down the page like the queue, so it shortens like the
    /// queue. The exact figure of a past decision is in the audit log, and of a
    /// pending one on the card — neither is this row's job.
    #[test]
    fn activity_shortens_the_amount_it_shows() {
        let mut rich = history("r1", NOW - 120, OutcomeState::Executed);
        rich.to = Some("0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb".to_owned());
        rich.amount_wei = Some("5499068022390730".to_owned());
        let m = on_activity(vec![rich]);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["approved", "0.005499… ETH"]),
            "the row is scannable:\n{rows:#?}"
        );
    }

    #[test]
    fn a_rich_row_shows_age_outcome_amount_and_a_shortened_address() {
        let mut rich = history("r1", NOW - 120, OutcomeState::Executed);
        rich.to = Some("0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb".to_owned());
        rich.amount_wei = Some("10000000000000000".to_owned());
        rich.tx_hash = Some("0xfeedfeedfeedfeed".to_owned());
        let m = on_activity(vec![rich]);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(
                &rows,
                &[
                    "2m ago",
                    "approved",
                    "0.01 ETH",
                    "0x489Fe0…bbbb",
                    "tx 0xfeedfe…feed"
                ]
            ),
            "age, outcome, human amount, SHORTENED address and tx on one row: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("0x489Fe09Fbb489Fe09Fbb")),
            "the Activity list never carries the full address — display, not signing"
        );
    }

    #[test]
    fn a_server_only_row_admits_it_has_no_details() {
        let m = on_activity(vec![history("s1", NOW - 3600, OutcomeState::Expired)]);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["1h ago", "expired", "(details not recorded)"]),
            "a poor record renders honestly, no fabricated columns: {rows:?}"
        );
    }

    #[test]
    fn an_empty_history_and_an_empty_filter_speak_up() {
        let mut m = on_activity(vec![]);
        let rows = draw_rows(&m, 80, 24);
        assert!(has_line_with(&rows, &["no activity yet"]));
        assert!(
            has_line_with(&rows, &["filter: all", "[f cycles]"]),
            "the filter header names the active filter: {rows:?}"
        );

        // One denied entry, filter cycled to `executed`: nothing matches.
        m.set_history(vec![history("d1", NOW - 5, OutcomeState::Denied)]);
        m.update(Msg::Filter);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["no executed outcomes under this filter"]),
            "an empty filtered view says which filter hides the rows: {rows:?}"
        );
    }

    #[test]
    fn every_outcome_word_carries_its_semantic_color() {
        // Gate-2 НИТ: all four states, not just Denied.
        use ratatui::backend::TestBackend;
        let m = on_activity(vec![
            history("e1", NOW - 2, OutcomeState::Executed),
            history("d1", NOW - 5, OutcomeState::Denied),
            history("x1", NOW - 9, OutcomeState::Expired),
            history("f1", NOW - 13, OutcomeState::Failed),
        ]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, &m, NOW, Versions::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (word, color) in [
            ("approved", theme::approve()),
            ("rejected", theme::reject()),
            ("expired", theme::high_risk()),
            ("failed", theme::reject()),
        ] {
            let mut found = false;
            for y in 0..24u16 {
                let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                if let Some(at) = row.find(word) {
                    assert_eq!(
                        buffer[(u16::try_from(at).unwrap(), y)].style().fg,
                        Some(color),
                        "{word} reads in its semantic color"
                    );
                    found = true;
                }
            }
            assert!(found, "the {word} row renders");
        }
    }

    #[test]
    fn overflowing_history_ends_with_an_honest_marker() {
        // 80×24: 1 tab row + 2 borders leave 21 inner rows; the filter header
        // takes 1 → a 20-row budget. Exactly 20 entries fit without a marker;
        // 21 show 19 + "+2 more" (the Stage-5 exact-fit/marker split).
        let exact: Vec<HistoryEntry> = (0u64..20)
            .map(|i| history(&format!("e{i:02}"), NOW - i, OutcomeState::Denied))
            .collect();
        let m = on_activity(exact);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            !rows.iter().any(|r| r.contains("more")),
            "an exact fit needs no marker: {rows:?}"
        );

        let over: Vec<HistoryEntry> = (0u64..21)
            .map(|i| history(&format!("e{i:02}"), NOW - i, OutcomeState::Denied))
            .collect();
        let m = on_activity(over);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["+2 more"]),
            "one over budget keeps budget−1 rows and says what is hidden: {rows:?}"
        );
    }

    #[test]
    fn the_tab_bar_carries_the_activity_tab() {
        let mut m = Model::new();
        to_watching(&mut m, vec![]);
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["Queue·0 [a]", "Receive [r]", "Activity [h]"]),
            "all four tabs share the 80-column header row: {rows:?}"
        );
    }

    #[test]
    fn the_history_note_renders_as_a_footer() {
        let mut m = on_activity(vec![]);
        m.set_history_note("history is session-only — set RUSTOK_CONSOLE_LOG".to_owned());
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["session-only"]),
            "log degradation is visible on the view: {rows:?}"
        );
    }

    #[test]
    fn the_note_and_the_overflow_marker_share_the_budget() {
        // Gate-2 НИТ: the note reserves its row BEFORE the budget is spent —
        // 21 inner rows − header − note = 19 → 18 rows + "+3 more", and the
        // note still visible under the marker.
        let over: Vec<HistoryEntry> = (0u64..21)
            .map(|i| history(&format!("e{i:02}"), NOW - i, OutcomeState::Denied))
            .collect();
        let mut m = on_activity(over);
        m.set_history_note("history is session-only".to_owned());
        let rows = draw_rows(&m, 80, 24);
        assert!(
            has_line_with(&rows, &["+3 more"]),
            "the note's reserved row shrinks the list budget by one: {rows:?}"
        );
        assert!(
            has_line_with(&rows, &["session-only"]),
            "the note survives an overflowing list: {rows:?}"
        );
    }

    // ─── The mode switcher (spec §2.4) — the overlay itself, on a test backend ───

    /// A model standing on the Dashboard with a real policy pair — the same
    /// message path the app takes, no struct built by hand.
    fn to_dashboard_with_policy(
        m: &mut Model,
        mode: crate::protocol::PolicyMode,
        origin: crate::protocol::PolicyOrigin,
    ) {
        to_watching(m, vec![]);
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            crate::protocol::WalletContext {
                address: "0x742d35Cc6634C0532925a3b844Bc454e4438f44e".to_owned(),
                balances: vec![],
                unavailable: vec![],
                allowed_chains: vec![1],
                policy: Policy { mode, origin },
            },
        )))));
        m.update(Msg::View(crate::app::View::Dashboard));
    }

    #[test]
    fn the_switcher_lists_three_modes_and_marks_the_current() {
        let mut m = Model::new();
        to_dashboard_with_policy(
            &mut m,
            crate::protocol::PolicyMode::Supervised,
            crate::protocol::PolicyOrigin::Acknowledged,
        );
        m.update(Msg::SwitchStart);
        let rows = draw_rows(&m, 80, 24);
        let screen = rows.join("\n");
        for word in ["read_only", "supervised", "autonomous"] {
            assert!(screen.contains(word), "the overlay lists {word}");
        }
        let current_row = rows
            .iter()
            .find(|r| r.contains("(current)"))
            .expect("the current mode is marked");
        assert!(
            current_row.contains("supervised"),
            "the mark sits on the wallet's own mode: {current_row}"
        );
        assert!(
            screen.contains("choose"),
            "the picking stage names its keys"
        );
        m.update(Msg::SwitchNext); // the selector moves; the mark must not follow
        let rows = draw_rows(&m, 80, 24);
        let current_row = rows
            .iter()
            .find(|r| r.contains("(current)"))
            .expect("the mark survives a selector move");
        assert!(
            current_row.contains("supervised"),
            "the mark stays on the wallet's own mode, not the cursor: {current_row}"
        );
    }

    #[test]
    fn the_disclaimer_renders_exactly_when_autonomous_is_selected() {
        const ANCHOR: &str = "ceiling is the wallet balance";
        let mut m = Model::new();
        to_dashboard_with_policy(
            &mut m,
            crate::protocol::PolicyMode::Supervised,
            crate::protocol::PolicyOrigin::Acknowledged,
        );
        m.update(Msg::SwitchStart);
        assert!(
            !draw(&m, 80, 24).contains(ANCHOR),
            "no disclaimer while supervised is selected"
        );
        m.update(Msg::SwitchNext); // supervised -> autonomous
        assert!(
            draw(&m, 80, 24).contains(ANCHOR),
            "the disclaimer renders the moment autonomous is selected"
        );
        m.update(Msg::PinSubmit); // PIN stage opens
        assert!(
            draw(&m, 80, 24).contains(ANCHOR),
            "and stays on screen while the PIN is typed — read before AND during"
        );
    }

    #[test]
    fn the_switcher_pin_stage_masks_digits_like_every_other_pin() {
        let mut m = Model::new();
        to_dashboard_with_policy(
            &mut m,
            crate::protocol::PolicyMode::Supervised,
            crate::protocol::PolicyOrigin::Acknowledged,
        );
        m.update(Msg::SwitchStart);
        m.update(Msg::PinSubmit);
        for c in "4839".chars() {
            m.update(Msg::PinDigit(c));
        }
        let screen = draw(&m, 80, 24);
        assert!(screen.contains("●●●●"), "four dots for four digits");
        assert!(!screen.contains("4839"), "the digits must never render");
        assert!(screen.contains("apply"), "the PIN stage names its keys");
    }
}
