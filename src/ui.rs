//! Render helpers for the ratatui front-end.
//!
//! Everything here is pure: it turns plain data (entries, status, layout
//! inputs) into ratatui [`Line`]s and [`Rect`]s and never touches a terminal,
//! a channel or the agent. That keeps the visuals unit-testable and lets
//! `src/bin/tui.rs` stay a thin event/input shell.
//!
//! The look we are after is the Claude Code CLI transcript: a `⏺` gutter for
//! tool calls with indented results hanging off a `⎿`, dim system notices, and
//! a compact status line above a single-line-ish input box.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Gutter marker for a tool call.
pub const TOOL_GUTTER: &str = "\u{23fa}";
/// Gutter marker for a tool result / continuation line.
pub const CONT_GUTTER: &str = "\u{23bf}";
/// Prompt marker for user input.
pub const PROMPT_GUTTER: &str = ">";

/// Braille spinner frames, in order.
pub const SPINNER: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

/// The spinner frame for a monotonically increasing tick.
pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

/// What a transcript entry represents. Drives both styling and gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A prompt typed by the human.
    User,
    /// Prose produced by the model.
    Assistant,
    /// The model asked for a tool.
    ToolCall,
    /// The harness ran it and got something back.
    ToolResult,
    /// A lifecycle line from another subsystem (memory, cron, team, ...).
    Notice,
    /// Something went wrong.
    Error,
}

/// One block of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What kind of block this is.
    pub kind: EntryKind,
    /// Short headline: a tool name, a subsystem tag, or empty.
    pub title: String,
    /// The payload, already stringified by the caller.
    pub body: String,
    /// True while the tool call is still in flight (spinner instead of gutter).
    pub running: bool,
}

impl Entry {
    /// A prompt from the human.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::User,
            title: String::new(),
            body: text.into(),
            running: false,
        }
    }

    /// Model prose.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Assistant,
            title: String::new(),
            body: text.into(),
            running: false,
        }
    }

    /// A tool call, marked as still running.
    pub fn tool_call(name: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::ToolCall,
            title: name.into(),
            body: input.into(),
            running: true,
        }
    }

    /// The result of a tool call.
    pub fn tool_result(name: impl Into<String>, output: impl Into<String>, is_error: bool) -> Self {
        Self {
            kind: if is_error {
                EntryKind::Error
            } else {
                EntryKind::ToolResult
            },
            title: name.into(),
            body: output.into(),
            running: false,
        }
    }

    /// A one-line system notice.
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Notice,
            title: String::new(),
            body: text.into(),
            running: false,
        }
    }

    /// An error line.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Error,
            title: String::new(),
            body: text.into(),
            running: false,
        }
    }

    /// Settle a running call (the tool came back).
    pub fn settled(mut self) -> Self {
        self.running = false;
        self
    }

    /// True for blocks that carry a gutter marker.
    pub fn has_gutter(&self) -> bool {
        matches!(
            self.kind,
            EntryKind::User | EntryKind::ToolCall | EntryKind::ToolResult | EntryKind::Error
        )
    }
}

/// Every colour the UI uses, in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Human prompts.
    pub user: Style,
    /// Model prose.
    pub assistant: Style,
    /// Tool names.
    pub tool: Style,
    /// Tool results.
    pub tool_output: Style,
    /// Tool failures and other errors.
    pub error: Style,
    /// Subsystem notices.
    pub notice: Style,
    /// De-emphasised chrome (gutters, hints, counts).
    pub dim: Style,
    /// The `⏺`/`⎿` markers.
    pub gutter: Style,
    /// Highlighted bits (spinner, selected row).
    pub accent: Style,
    /// Box borders.
    pub border: Style,
    /// The `> ` prompt.
    pub prompt: Style,
    /// The hint row under the input.
    pub hint: Style,
    /// The startup banner.
    pub banner: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            assistant: Style::new().fg(Color::Gray),
            tool: Style::new().fg(Color::Cyan),
            tool_output: Style::new().fg(Color::DarkGray),
            error: Style::new().fg(Color::Red),
            notice: Style::new().fg(Color::Yellow),
            dim: Style::new().fg(Color::DarkGray),
            gutter: Style::new().fg(Color::DarkGray),
            accent: Style::new().fg(Color::LightMagenta),
            border: Style::new().fg(Color::DarkGray),
            prompt: Style::new()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
            hint: Style::new().fg(Color::DarkGray),
            banner: Style::new()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        }
    }
}

/// The facts behind the status row.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusInfo {
    /// Model id in use.
    pub model: String,
    /// Cumulative token count for the session.
    pub tokens: u64,
    /// Cumulative spend, if pricing is known.
    pub cost_usd: Option<f64>,
    /// Working directory, shortened on the way out.
    pub cwd: Option<String>,
    /// True while the agent thread is working.
    pub busy: bool,
    /// Free-form right-hand note (e.g. "2 subagents running").
    pub note: Option<String>,
}

impl StatusInfo {
    /// A status row for `model` with everything else zeroed.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            tokens: 0,
            cost_usd: None,
            cwd: None,
            busy: false,
            note: None,
        }
    }
}

/// A pending permission request, rendered as its own boxed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    /// Tool being requested.
    pub name: String,
    /// Its arguments, already compacted.
    pub input: String,
    /// Why the policy stopped it.
    pub reason: String,
}

impl ApprovalView {
    /// Build a view for a pending call.
    pub fn new(
        name: impl Into<String>,
        input: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            input: input.into(),
            reason: reason.into(),
        }
    }
}

/// Where each band of the screen lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiLayout {
    /// Wordmark, only on tall terminals.
    pub banner: Option<Rect>,
    /// Scrolling transcript.
    pub transcript: Rect,
    /// Permission prompt, when one is pending.
    pub approval: Option<Rect>,
    /// The input box.
    pub input: Rect,
    /// One line of model/token/cost/cwd.
    pub status: Rect,
    /// One line of key hints.
    pub hints: Rect,
}

/// Minimum rows before the wordmark is worth the space.
pub const BANNER_MIN_HEIGHT: u16 = 18;

/// Split a terminal area into the UI bands.
///
/// The fixed rows are laid out first (status, hints, input), then the banner
/// if the terminal is tall enough, then the approval box if one is pending;
/// whatever is left is the transcript.
///
/// `input_rows` is how many rows the (possibly wrapped) input needs; it is
/// clamped so the transcript always keeps at least one row and the input
/// never swallows the screen.
pub fn compute_layout(area: Rect, input_rows: u16, approval: Option<&ApprovalView>) -> UiLayout {
    let reserved = if area.height >= BANNER_MIN_HEIGHT {
        3
    } else {
        0
    };
    let input_rows = input_rows.clamp(1, area.height.saturating_sub(reserved + 3).clamp(1, 6));

    let mut constraints = Vec::new();
    if area.height >= BANNER_MIN_HEIGHT {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Min(1));
    if approval.is_some() {
        let rows = area.height.clamp(1, 7).saturating_sub(6).max(3);
        constraints.push(Constraint::Length(rows));
    }
    constraints.push(Constraint::Length(input_rows)); // input
    constraints.push(Constraint::Length(1)); // status
    constraints.push(Constraint::Length(1)); // hints

    let bands = Layout::vertical(constraints).split(area);
    let mut idx = 0;
    let banner = if area.height >= BANNER_MIN_HEIGHT {
        let rect = bands[idx];
        idx += 1;
        Some(rect)
    } else {
        None
    };
    let transcript = bands[idx];
    idx += 1;
    let approval_rect = approval.map(|_| {
        let rect = bands[idx];
        idx += 1;
        rect
    });
    let input = bands[idx];
    let status = bands[idx + 1];
    let hints = bands[idx + 2];

    UiLayout {
        banner,
        transcript,
        approval: approval_rect,
        input,
        status,
        hints,
    }
}

/// Display width of `text`, measured the way the renderer measures it.
pub fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Shorten `text` to at most `max` columns, ending in `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text_width(text) <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = text_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Hard-wrap `text` to `width` columns, preferring whitespace breaks.
///
/// Always returns at least one line so callers can rely on `lines[0]`.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut used = 0;
        for word in raw.split(' ') {
            let w = text_width(word);
            if used > 0 && used + 1 + w > width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            if w > width {
                // Long token (path, JSON blob): break it mid-word.
                if used > 0 {
                    lines.push(std::mem::take(&mut current));
                }
                let mut piece = String::new();
                let mut piece_w = 0;
                for ch in word.chars() {
                    let cw = text_width(&ch.to_string());
                    if piece_w + cw > width {
                        lines.push(std::mem::take(&mut piece));
                        piece_w = 0;
                    }
                    piece.push(ch);
                    piece_w += cw;
                }
                current = piece;
                used = piece_w;
                continue;
            }
            if used > 0 {
                current.push(' ');
                used += 1;
            }
            current.push_str(word);
            used += w;
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Leading indent used for continuation lines.
const CONT_INDENT: usize = 2;

fn indent_line(line: Line<'static>, width: usize) -> Line<'static> {
    let indent = CONT_INDENT.min(width);
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Render one transcript entry.
pub fn entry_lines(entry: &Entry, theme: &Theme, width: usize, tick: usize) -> Vec<Line<'static>> {
    match entry.kind {
        EntryKind::User => {
            let body = truncate(&entry.body.replace('\n', " "), width.saturating_sub(2));
            vec![Line::from(vec![
                Span::styled(format!("{} ", PROMPT_GUTTER), theme.prompt),
                Span::styled(body, theme.user),
            ])]
        }
        EntryKind::Assistant => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, part) in wrap_text(&entry.body, width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                let line = Line::from(Span::styled(part, theme.assistant));
                lines.push(if i == 0 {
                    line
                } else {
                    indent_line(line, width)
                });
            }
            lines
        }
        EntryKind::ToolCall => {
            let marker = if entry.running {
                Span::styled(format!("{} ", spinner_frame(tick)), theme.accent)
            } else {
                Span::styled(format!("{TOOL_GUTTER} "), theme.gutter)
            };
            let name = Span::styled(entry.title.clone(), theme.tool);
            let preview = truncate(
                &entry.body.replace('\n', " "),
                width.saturating_sub(text_width(&entry.title) + 3),
            );
            let mut spans = vec![marker, name];
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" {preview}"), theme.dim));
            }
            vec![Line::from(spans)]
        }
        EntryKind::ToolResult | EntryKind::Error => {
            let style = if entry.kind == EntryKind::Error {
                theme.error
            } else {
                theme.tool_output
            };
            let head = if entry.title.is_empty() {
                String::new()
            } else {
                format!("{} ", truncate(&entry.title, width.saturating_sub(4)))
            };
            let mut lines = Vec::new();
            let body_lines = wrap_text(&entry.body, width.saturating_sub(CONT_INDENT + 2).max(1));
            for (i, part) in body_lines.into_iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{CONT_GUTTER} "), theme.gutter),
                        Span::styled(head.clone(), style),
                        Span::styled(part, style),
                    ]));
                } else {
                    lines.push(indent_line(Line::from(Span::styled(part, style)), width));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("{CONT_GUTTER} {head}"),
                    style,
                )));
            }
            lines
        }
        EntryKind::Notice => vec![Line::from(Span::styled(
            format!("  {}", truncate(&entry.body, width.saturating_sub(2))),
            theme.notice,
        ))],
    }
}

/// The wordmark shown on tall terminals.
pub fn banner_lines(theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let wordmark = "  rust-agent";
    let tagline = "  a coding agent runtime \u{00b7} /help for commands";
    let rule = format!("  {}", "\u{2500}".repeat(width.saturating_sub(4).min(48)));
    vec![
        Line::from(Span::styled(truncate(wordmark, width), theme.banner)),
        Line::from(Span::styled(truncate(tagline, width), theme.dim)),
        Line::from(Span::styled(truncate(&rule, width), theme.border)),
    ]
}

/// The status row: spinner or idle dot, model, tokens, cost, cwd, note.
pub fn status_line(info: &StatusInfo, theme: &Theme, width: usize) -> Line<'static> {
    let lead = if info.busy {
        Span::styled(format!("{} ", spinner_frame(0)), theme.accent)
    } else {
        Span::styled("\u{00b7} ", theme.dim)
    };

    let mut right = Vec::new();
    if info.tokens > 0 {
        right.push(format!("{} tok", info.tokens));
    }
    if let Some(cost) = info.cost_usd {
        right.push(format!("${cost:.4}"));
    }
    if let Some(note) = &info.note {
        right.push(note.clone());
    }
    let right = right.join(" \u{00b7} ");

    let left_budget = width.saturating_sub(text_width(&right) + 2).max(1);
    let mut left = info.model.clone();
    if let Some(cwd) = &info.cwd {
        left = format!("{left}  {cwd}");
    }
    let left = truncate(&left, left_budget);

    let mut spans = vec![lead, Span::styled(left, theme.dim)];
    if !right.is_empty() {
        let pad = width
            .saturating_sub(
                text_width(&spans[0].content) + text_width(&spans[1].content) + text_width(&right),
            )
            .max(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right, theme.hint));
    }
    Line::from(spans)
}

/// The key hints under the input box.
pub fn hints_line(theme: &Theme, width: usize) -> Line<'static> {
    let hints = "enter send \u{00b7} shift+enter newline \u{00b7} esc quit \u{00b7} /help keys";
    Line::from(Span::styled(truncate(hints, width), theme.hint))
}

/// The input box: a `>` prompt and the buffer, wrapped, with a block cursor.
pub fn input_lines(buffer: &str, theme: &Theme, width: usize, focused: bool) -> Vec<Line<'static>> {
    let cursor = if focused { "\u{2588}" } else { "" };
    let text = format!("{buffer}{cursor}");
    let mut parts = wrap_text(&text, width.saturating_sub(2).max(1));
    if parts.is_empty() {
        parts.push(String::new());
    }
    let first = parts.remove(0);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{PROMPT_GUTTER} "), theme.prompt),
        Span::styled(first, theme.user),
    ])];
    for part in parts {
        lines.push(indent_line(
            Line::from(Span::styled(part, theme.user)),
            width,
        ));
    }
    lines
}

/// The pending-permission box.
pub fn approval_lines(
    approval: &ApprovalView,
    theme: &Theme,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(1);
    let mut lines = vec![Line::from(vec![
        Span::styled("  allow ", theme.accent),
        Span::styled(approval.name.clone(), theme.tool),
        Span::styled("?", theme.accent),
    ])];
    if height > 1 {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&approval.reason, inner)),
            theme.notice,
        )));
    }
    if height > 2 {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&approval.input.replace('\n', " "), inner)),
            theme.dim,
        )));
    }
    if height > 3 {
        lines.push(Line::from(Span::styled(
            "  [y] once   [a] always   [n] deny",
            theme.hint,
        )));
    }
    lines
}

/// The `/help` overlay, centred-ish and sized to the terminal.
pub fn help_overlay(theme: &Theme, width: usize, height: usize) -> Vec<Line<'static>> {
    let rows: [(&str, &str); 9] = [
        ("enter", "send the prompt"),
        ("shift+enter", "insert a newline"),
        ("esc", "quit, or close this overlay"),
        ("ctrl+c", "quit"),
        ("/goal", "set, inspect or clear the goal"),
        ("/reset", "clear the transcript"),
        ("/help", "toggle this overlay"),
        ("y / a / n", "allow once / always / deny"),
        ("f1", "open this overlay"),
    ];
    let mut lines = vec![Line::from(Span::styled("  keys", theme.banner))];
    for (key, what) in rows {
        if lines.len() + 1 >= height.saturating_sub(1) {
            break;
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>12}  ", key), theme.accent),
            Span::styled(truncate(what, width.saturating_sub(18)), theme.assistant),
        ]));
    }
    lines.push(Line::from(Span::styled("  esc to close", theme.hint)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    fn widest(lines: &[Line<'static>]) -> usize {
        lines.iter().map(Line::width).max().unwrap_or(0)
    }

    #[test]
    fn spinner_cycles_and_wraps() {
        assert_eq!(spinner_frame(0), SPINNER[0]);
        assert_eq!(spinner_frame(3), SPINNER[3]);
        assert_eq!(spinner_frame(SPINNER.len()), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER.len() + 7), SPINNER[7]);
    }

    #[test]
    fn wrap_respects_width_and_keeps_content() {
        let text = "the quick brown fox jumps over the lazy dog and keeps going";
        for width in [8usize, 16, 24, 40] {
            let lines = wrap_text(text, width);
            assert!(
                lines.iter().all(|l| text_width(l) <= width),
                "{width}: {lines:?}"
            );
            let joined = lines.join(" ");
            for word in text.split(' ') {
                assert!(joined.contains(word), "lost {word:?} at width {width}");
            }
        }
    }

    #[test]
    fn wrap_breaks_tokens_longer_than_the_line() {
        let lines = wrap_text("a/very/long/unbreakable/path/that/never/ends", 10);
        assert!(lines.iter().all(|l| text_width(l) <= 10), "{lines:?}");
        assert!(lines.len() > 1);
        assert_eq!(
            lines.concat().replace(' ', ""),
            "a/very/long/unbreakable/path/that/never/ends"
        );
    }

    #[test]
    fn wrap_keeps_blank_lines() {
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w\u{2026}");
        assert!(text_width(&truncate("hello world", 8)) <= 8);
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_counts_wide_glyphs_by_column() {
        // Each CJK glyph is two columns wide.
        let cut = truncate("\u{4f60}\u{597d}\u{4e16}\u{754c}", 5);
        assert!(text_width(&cut) <= 5, "{cut:?}");
        assert_eq!(cut, "\u{4f60}\u{597d}\u{2026}");
    }

    #[test]
    fn layout_bands_never_overlap_and_always_fit() {
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (40, 12), (30, 8)] {
            let area = Rect::new(0, 0, w, h);
            let l = compute_layout(area, 1, None);
            let mut used = 0u16;
            if let Some(b) = l.banner {
                assert_eq!(b.height, 3);
                used += b.height;
            }
            used += l.transcript.height + l.input.height + l.status.height + l.hints.height;
            assert_eq!(used, h, "{w}x{h} wasted rows");
            assert!(l.transcript.height >= 1, "{w}x{h} no transcript");
            assert_eq!(l.approval, None);
        }
    }

    #[test]
    fn banner_only_appears_on_tall_terminals() {
        assert!(compute_layout(Rect::new(0, 0, 80, 17), 1, None)
            .banner
            .is_none());
        assert!(compute_layout(Rect::new(0, 0, 80, 18), 1, None)
            .banner
            .is_some());
    }

    #[test]
    fn input_band_grows_but_keeps_the_transcript_alive() {
        let tall = compute_layout(Rect::new(0, 0, 100, 30), 4, None);
        assert_eq!(tall.input.height, 4);
        assert!(tall.transcript.height >= 1);
        // A short terminal clamps the input instead of eating the transcript.
        let tiny = compute_layout(Rect::new(0, 0, 40, 6), 6, None);
        assert!(tiny.transcript.height >= 1, "{tiny:?}");
        assert!(tiny.input.height < 6, "{tiny:?}");
    }

    #[test]
    fn approval_band_steals_from_the_transcript_and_gives_it_back() {
        let area = Rect::new(0, 0, 100, 30);
        let plain = compute_layout(area, 1, None);
        let gate = ApprovalView::new("bash", "rm -rf target", "destructive");
        let gated = compute_layout(area, 1, Some(&gate));
        let approval = gated.approval.expect("approval band");
        assert!(approval.height >= 3);
        assert_eq!(
            gated.transcript.height + approval.height,
            plain.transcript.height
        );
        assert_eq!(gated.transcript.y, plain.transcript.y);
        assert_eq!(gated.input.y, plain.input.y);
    }

    #[test]
    fn every_entry_kind_stays_inside_its_width() {
        let theme = theme();
        let entries = vec![
            Entry::user("refactor the llm client and keep the tests green while you are at it"),
            Entry::assistant("Sure. The client currently\nsplits the response into blocks."),
            Entry::tool_call("bash", "cargo test --lib -- --nocapture"),
            Entry::tool_result("bash", "test result: ok. 214 passed; 0 failed", false),
            Entry::tool_result("bash", "error: could not compile `tui`", true),
            Entry::notice("[memory] recalled 3 notes"),
            Entry::error("rate limited, retrying in 2s"),
        ];
        for (w, h) in [(80usize, 24usize), (100, 30), (120, 40)] {
            for entry in &entries {
                let lines = entry_lines(entry, &theme, w, 4);
                assert!(!lines.is_empty(), "{:?} rendered nothing", entry.kind);
                assert!(
                    widest(&lines) <= w,
                    "{:?} overflowed {w} with {:?}",
                    entry.kind,
                    lines
                );
            }
            let status = status_line(
                &StatusInfo {
                    model: "claude-sonnet-4-5".into(),
                    tokens: 12345,
                    cost_usd: Some(0.0345),
                    cwd: Some("/d/Agent/Rust-agent".into()),
                    busy: true,
                    note: Some("2 subagents".into()),
                },
                &theme,
                w,
            );
            assert!(status.width() <= w, "status overflow at {w}");
            assert!(hints_line(&theme, w).width() <= w);
            assert!(banner_lines(&theme, w).iter().all(|l| l.width() <= w));
            assert!(input_lines("hello", &theme, w, true)
                .iter()
                .all(|l| l.width() <= w));
            assert!(help_overlay(&theme, w, h).iter().all(|l| l.width() <= w));
            let gate = ApprovalView::new("bash", "rm -rf target", "destructive command");
            assert!(approval_lines(&gate, &theme, w, 4)
                .iter()
                .all(|l| l.width() <= w));
        }
    }

    #[test]
    fn tool_call_and_result_use_distinct_gutters() {
        let theme = theme();
        let call = entry_lines(
            &Entry::tool_call("read", "src/lib.rs").settled(),
            &theme,
            80,
            0,
        );
        assert!(call[0].to_string().contains(TOOL_GUTTER));
        let running = entry_lines(&Entry::tool_call("read", "src/lib.rs"), &theme, 80, 2);
        assert!(running[0].to_string().contains(spinner_frame(2)));
        let done = entry_lines(
            &Entry::tool_result("read", "1  //! docs", false),
            &theme,
            80,
            0,
        );
        assert!(done[0].to_string().contains(CONT_GUTTER));
    }

    #[test]
    fn user_entries_are_collapsed_to_one_line() {
        let lines = entry_lines(&Entry::user("first\nsecond"), &theme(), 80, 0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("first second"));
    }

    #[test]
    fn input_lines_wrap_and_show_a_cursor_only_when_focused() {
        let focused = input_lines("a long enough buffer to wrap", &theme(), 12, true);
        assert!(focused.len() > 1);
        assert!(focused.iter().all(|l| l.width() <= 12));
        assert!(focused[0].to_string().contains(PROMPT_GUTTER));
        let blurred = input_lines("hi", &theme(), 80, false);
        assert_eq!(blurred.len(), 1);
        assert!(!blurred[0].to_string().contains('\u{2588}'));
    }

    #[test]
    fn status_line_right_aligns_the_counters() {
        let info = StatusInfo {
            model: "m".into(),
            tokens: 10,
            cost_usd: None,
            cwd: None,
            busy: false,
            note: None,
        };
        let line = status_line(&info, &theme(), 40);
        assert_eq!(line.width(), 40);
        assert!(line.to_string().trim_end().ends_with("10 tok"));
    }

    #[test]
    fn settled_clears_the_spinner() {
        let entry = Entry::tool_call("bash", "ls").settled();
        assert!(!entry.running);
        assert!(entry_lines(&entry, &theme(), 80, 3)[0]
            .to_string()
            .contains(TOOL_GUTTER));
    }
}
