//! Drawing: the transcript, the queue, the activity line, the editor and the status line.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    Activity, App, COPY_NOTE, Cached, CompactState, CompactionView, Entry, NoteKind, Placed, Seen,
};
use super::login::{Login, MODEL_ROWS, Stage, matching};
use super::permissions::{ADD_HELP, Kind, Permissions};
use super::sessions::{ROWS as SESSION_ROWS, SessionPicker, age};
use super::settings::{ROWS, SettingsDialog};
use super::tree::{ROWS as TREE_ROWS, Tree};
use crate::tui::status::StatusData;
use crate::tui::syntax::{self, Language, State};
use crate::tui::theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Output lines a collapsed tool block shows.
const PREVIEW_LINES: usize = 5;
const MAX_EDITOR_ROWS: usize = 10;

/// One logical line: a prefix for its first visual row, a prefix for the rows it wraps onto, and
/// its content.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub first: Vec<Span<'static>>,
    pub rest: Vec<Span<'static>>,
    pub spans: Vec<Span<'static>>,
}

impl Row {
    fn plain(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            ..Self::default()
        }
    }

    fn prefixed(first: &str, rest: &str, style: Style, spans: Vec<Span<'static>>) -> Self {
        Self {
            first: vec![Span::styled(first.to_owned(), style)],
            rest: vec![Span::styled(rest.to_owned(), style)],
            spans,
        }
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Word-wraps a row to `width` columns. Leading whitespace on the first row is kept (code
/// indentation); whitespace at a break is dropped; a word longer than a row is split.
pub fn wrap(row: &Row, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = row.first.clone();
    let mut limit = width.saturating_sub(spans_width(&row.first)).max(1);
    let mut used = 0;
    let mut wrapped = false;

    let push = |current: &mut Vec<Span<'static>>, text: &str, style: Style| {
        if let Some(last) = current.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
            return;
        }
        current.push(Span::styled(text.to_owned(), style));
    };

    for span in &row.spans {
        let style = span.style;
        for piece in pieces(&span.content) {
            let w = piece.width();
            let space = piece.starts_with(char::is_whitespace);
            if space {
                if used == 0 && wrapped {
                    continue;
                }
                if used + w > limit {
                    lines.push(Line::from(std::mem::replace(
                        &mut current,
                        row.rest.clone(),
                    )));
                    limit = width.saturating_sub(spans_width(&row.rest)).max(1);
                    used = 0;
                    wrapped = true;
                    continue;
                }
                push(&mut current, piece, style);
                used += w;
                continue;
            }
            if used + w > limit && used > 0 {
                lines.push(Line::from(std::mem::replace(
                    &mut current,
                    row.rest.clone(),
                )));
                limit = width.saturating_sub(spans_width(&row.rest)).max(1);
                used = 0;
                wrapped = true;
            }
            if w <= limit {
                push(&mut current, piece, style);
                used += w;
                continue;
            }
            for c in piece.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > limit && used > 0 {
                    lines.push(Line::from(std::mem::replace(
                        &mut current,
                        row.rest.clone(),
                    )));
                    limit = width.saturating_sub(spans_width(&row.rest)).max(1);
                    used = 0;
                    wrapped = true;
                }
                push(&mut current, c.encode_utf8(&mut [0; 4]), style);
                used += cw;
            }
        }
    }
    lines.push(Line::from(current));
    lines
}

/// Splits text into alternating runs of whitespace and non-whitespace.
fn pieces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (i, c) in s.char_indices() {
        let space = c.is_whitespace();
        if in_space.is_some_and(|prev| prev != space) {
            out.push(&s[start..i]);
            start = i;
        }
        in_space = Some(space);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn code_style(theme: &Theme) -> Style {
    Style::new().fg(theme.md_code)
}

/// A fenced block, which pi colours separately from inline code (D72).
fn block_style(theme: &Theme) -> Style {
    Style::new().fg(theme.md_code_block)
}

/// Markdown to rows, fitted to `room` columns. Soft breaks are kept as line breaks, since a model's
/// line breaks are usually deliberate (tables, lists written by hand).
///
/// The room is what a table needs and nothing else does: a table is the one construct whose layout
/// cannot survive being wrapped afterwards, because a wrapped row loses the alignment that makes
/// it a table. Every other block is wrapped by [`wrap`] as it always was.
pub fn markdown(text: &str, theme: &Theme, room: usize) -> Vec<Row> {
    markdown_over(text, theme, Style::new(), room)
}

/// Markdown drawn over `base`, which is what a message the user typed gets: the text is already in
/// their colour, and the emphasis they wrote is a modifier on top of it.
fn markdown_over(text: &str, theme: &Theme, base: Style, room: usize) -> Vec<Row> {
    let mut md = Markdown {
        theme: theme.clone(),
        base,
        room,
        ..Markdown::default()
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        md.event(event);
    }
    md.flush();
    md.rows
}

#[derive(Default)]
struct Markdown {
    /// The columns a table may use, so it is fitted instead of wrapped apart.
    room: usize,
    /// The colours, kept here because the parser's callbacks are methods on this struct (D71).
    theme: Theme,
    rows: Vec<Row>,
    line: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// One entry per open list: the next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    bullet: Option<String>,
    quote: usize,
    code: bool,
    /// The language the open fence names, and the highlighter's carry-over state (D73).
    lang: Language,
    code_state: State,
    blank_before_next_block: bool,
    link: Option<String>,
    table: Option<Table>,
    /// The style everything is drawn over: empty for a reply, the prompt's own colour for a message
    /// the user typed, so the emphasis inside it is visible against their text (D81).
    base: Style,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    head_rows: usize,
}

impl Markdown {
    fn code_style(&self) -> Style {
        self.base.patch(code_style(&self.theme))
    }

    fn block_style(&self) -> Style {
        block_style(&self.theme)
    }

    fn style(&self) -> Style {
        self.styles.iter().fold(self.base, |acc, s| acc.patch(*s))
    }

    fn indent(&self) -> String {
        "  ".repeat(self.lists.len())
    }

    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let quote = "│ ".repeat(self.quote);
        let quote_style = Style::new().fg(self.theme.md_quote_border);
        let indent = self.indent();
        let first = match self.bullet.take() {
            Some(bullet) => {
                let depth = self.lists.len().saturating_sub(1);
                let lead = format!("{}{bullet}", "  ".repeat(depth));
                // Continuation lines line up with the text after the bullet.
                let pad = lead.width().max(indent.width());
                let rest = " ".repeat(pad);
                return (
                    vec![
                        Span::styled(quote.clone(), quote_style),
                        Span::styled(lead, Style::new().fg(self.theme.md_list_bullet)),
                    ],
                    vec![Span::styled(quote, quote_style), Span::raw(rest)],
                );
            }
            None => indent.clone(),
        };
        (
            vec![Span::styled(quote.clone(), quote_style), Span::raw(first)],
            vec![Span::styled(quote, quote_style), Span::raw(indent)],
        )
    }

    /// Ends the current line. A pending bullet waits for the first line with text.
    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let (first, rest) = self.prefixes();
        let spans = std::mem::take(&mut self.line);
        self.rows.push(Row { first, rest, spans });
    }

    fn start_block(&mut self) {
        self.flush();
        if self.blank_before_next_block && !self.rows.is_empty() && self.lists.is_empty() {
            self.rows.push(Row::default());
        }
        self.blank_before_next_block = false;
    }

    fn text(&mut self, s: &str, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                cell.push(Span::styled(s.to_owned(), style));
            }
            return;
        }
        self.line.push(Span::styled(s.to_owned(), style));
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) if self.code => {
                let mut lines = t.split('\n').peekable();
                while let Some(l) = lines.next() {
                    if l.is_empty() && lines.peek().is_none() {
                        break;
                    }
                    // The block's own colour is the base; each run paints over it with its role
                    // wherever the language is known (D73).
                    self.line
                        .push(Span::styled("  ".to_owned(), self.block_style()));
                    for (kind, text) in syntax::highlight(&self.lang, l, &mut self.code_state) {
                        let style = self.block_style().patch(kind.style(&self.theme.syntax));
                        self.line.push(Span::styled(text.to_owned(), style));
                    }
                    self.flush();
                }
            }
            Event::Text(t) => {
                let style = self.style();
                self.text(&t, style);
            }
            Event::Code(t) => self.text(&t, self.code_style()),
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => {
                self.start_block();
                self.line.push(Span::styled(
                    "─".repeat(40),
                    Style::new().fg(self.theme.md_hr),
                ));
                self.flush();
                self.blank_before_next_block = true;
            }
            Event::TaskListMarker(done) => {
                self.line
                    .push(Span::raw(if done { "[x] " } else { "[ ] " }));
            }
            Event::Html(t)
            | Event::InlineHtml(t)
            | Event::InlineMath(t)
            | Event::DisplayMath(t) => {
                let style = self.style();
                self.text(t.trim_end_matches('\n'), style);
            }
            Event::FootnoteReference(t) => {
                let style = self.style();
                self.text(&format!("[^{t}]"), style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.start_block();
                } else {
                    self.flush();
                }
            }
            Tag::Heading { level, .. } => {
                self.start_block();
                let style = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => Style::new()
                        .add_modifier(Modifier::BOLD)
                        .fg(self.theme.md_heading),
                    _ => Style::new().add_modifier(Modifier::BOLD),
                };
                self.styles.push(style);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                self.code = true;
                self.lang = match kind {
                    CodeBlockKind::Fenced(tag) => Language::parse(&tag),
                    // An indented block names no language, so there is nothing to look up.
                    CodeBlockKind::Indented => Language::Plain,
                };
                self.code_state = State::default();
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                self.quote += 1;
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.start_block();
                } else {
                    self.flush();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let b = format!("{n}. ");
                        *n += 1;
                        b
                    }
                    _ => "• ".to_owned(),
                };
                self.bullet = Some(bullet);
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(
                    Style::new()
                        .add_modifier(Modifier::UNDERLINED)
                        .fg(self.theme.md_link),
                );
                self.link = Some(dest_url.to_string());
            }
            Tag::Table(_) => {
                self.start_block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.blank_before_next_block = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.blank_before_next_block = true;
            }
            TagEnd::CodeBlock => {
                self.code = false;
                self.lang = Language::Plain;
                self.code_state = State::default();
                self.flush();
                self.blank_before_next_block = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.blank_before_next_block = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.blank_before_next_block = true;
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    let shown: String = self.line.iter().map(|s| s.content.as_ref()).collect();
                    if !url.is_empty() && !shown.ends_with(&url) {
                        self.line.push(Span::styled(
                            format!(" <{url}>"),
                            Style::new().fg(self.theme.muted),
                        ));
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head_rows = t.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_rows(t);
                }
                self.blank_before_next_block = true;
            }
            _ => {}
        }
    }

    /// Lays a table out inside the room it was given.
    ///
    /// The widths are the natural ones when they fit, and otherwise shrunk towards a minimum, which
    /// is what pi does with the same problem: a column's share falls in proportion to how much it
    /// wanted above its minimum, so a table of one wide prose column and three short ones keeps the
    /// short ones readable instead of squeezing every column equally.
    ///
    /// A cell is then wrapped inside its column and padded to it, so a row that has to be tall
    /// stays aligned. That is the whole point: a table whose rows are wrapped by the generic
    /// wrapper afterwards is not a table any more.
    fn table_rows(&mut self, t: Table) {
        let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut natural = vec![0; cols];
        let mut longest_word = vec![1; cols];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                natural[i] = natural[i].max(spans_width(cell));
                longest_word[i] = longest_word[i].max(longest_word_width(cell));
            }
        }
        let widths = column_widths(&natural, &longest_word, self.room);
        let sep = Style::new().fg(self.theme.muted);
        for (r, row) in t.rows.into_iter().enumerate() {
            let head = r < t.head_rows;
            // Every cell wrapped to its column, then the row read across the tallest of them.
            let cells: Vec<Vec<Vec<Span<'static>>>> = (0..cols)
                .map(|i| match row.get(i) {
                    Some(cell) => wrap_cell(cell, widths[i], head),
                    None => vec![Vec::new()],
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for line in 0..height {
                for (i, cell) in cells.iter().enumerate() {
                    if i > 0 {
                        self.line.push(Span::styled(" │ ", sep));
                    }
                    let spans = cell.get(line).cloned().unwrap_or_default();
                    let pad = widths[i].saturating_sub(spans_width(&spans));
                    self.line.extend(spans);
                    self.line.push(Span::raw(" ".repeat(pad)));
                }
                self.flush();
            }
            if head && r + 1 == t.head_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.line.push(Span::styled(rule.join("─┼─"), sep));
                self.flush();
            }
        }
    }
}

/// The width of the longest word one cell cannot break, which is the narrowest its column may be
/// before a word has to be split.
fn longest_word_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .flat_map(|s| s.content.split_whitespace())
        .map(|w| w.width())
        .max()
        .unwrap_or(1)
}

/// Column widths for `natural` widths in `room` columns, separators included.
///
/// Fitting only ever shrinks: a table that already fits keeps its natural widths, so nothing moves
/// when the terminal is wide enough.
///
/// The shrink is water-filling rather than an even split, which is the difference between a table
/// that reads and one that does not. A column that would fit inside an equal share is given its
/// natural width and takes no part in the squeeze, so a `yes` column stays `yes` while the one
/// paragraph column beside it absorbs the whole deficit. An even split cuts the short word and
/// starves the column that could give most, which shows up as `saddl` / `e`.
///
/// `min` is the longest word per column, as this does what pi does: a word is only split when the
/// table cannot be laid out without splitting it, so `Plugin` stays whole and the column takes the
/// room from the prose beside it.
pub fn column_widths(natural: &[usize], min: &[usize], room: usize) -> Vec<usize> {
    let cols = natural.len();
    if cols == 0 {
        return Vec::new();
    }
    // " │ " between columns.
    let separators = 3 * cols.saturating_sub(1);
    let total: usize = natural.iter().sum();
    if total <= room.saturating_sub(separators) {
        return natural.to_vec();
    }
    let mut floors: Vec<usize> = (0..cols)
        .map(|i| min.get(i).copied().unwrap_or(1).min(natural[i]).max(1))
        .collect();
    // The room a column has once every other column has its floor, which is what decides whether
    // the floors themselves can be paid for.
    let floor_total: usize = floors.iter().sum();
    let budget = room.saturating_sub(separators).max(cols);
    if floor_total > budget {
        // Even the longest words do not fit: every column takes one cell and the rest is shared by
        // how much each still wants, so a wide table on a narrow screen still has a shape.
        floors = vec![1; cols];
        let mut widths = floors.clone();
        let want: Vec<usize> = natural.iter().map(|n| n.saturating_sub(1)).collect();
        let asked: usize = want.iter().sum();
        let spare = budget.saturating_sub(cols);
        for (i, w) in want.iter().enumerate() {
            widths[i] += (w * spare).checked_div(asked).unwrap_or(0);
        }
        return top_up(widths, natural, budget);
    }
    let mut widths = floors.clone();
    let mut order: Vec<usize> = (0..cols).collect();
    order.sort_by_key(|i| natural[*i]);
    let mut left = budget.saturating_sub(floor_total);
    let mut unresolved = cols;
    for (position, i) in order.iter().enumerate() {
        let want = natural[*i].saturating_sub(widths[*i]);
        let fair = left.checked_div(unresolved).unwrap_or(0);
        if want <= fair {
            widths[*i] += want;
            left -= want;
            unresolved -= 1;
        } else {
            // The rest share what is left: the wide columns are the ones that can give.
            let rest = &order[position..];
            let asked: usize = rest
                .iter()
                .map(|j| natural[*j].saturating_sub(widths[*j]))
                .sum();
            for j in rest {
                let want = natural[*j].saturating_sub(widths[*j]);
                let share = (want * left).checked_div(asked).unwrap_or(0);
                widths[*j] += share;
            }
            break;
        }
    }
    top_up(widths, natural, budget)
}

/// Hands out the columns integer division left over, to whichever column still has most to gain.
fn top_up(mut widths: Vec<usize>, natural: &[usize], budget: usize) -> Vec<usize> {
    let mut spare = budget.saturating_sub(widths.iter().sum::<usize>());
    while spare > 0 {
        let Some((i, _)) = widths
            .iter()
            .enumerate()
            .filter(|(i, w)| **w < natural[*i])
            .max_by_key(|(_, w)| **w)
        else {
            break;
        };
        widths[i] += 1;
        spare -= 1;
    }
    widths
}

/// One cell's spans wrapped to `width` columns, a line per row of the cell.
///
/// Words break at spaces and a word longer than the column is split, since a URL in a narrow column
/// must not push the rest of the row off the screen.
fn wrap_cell(spans: &[Span<'static>], width: usize, head: bool) -> Vec<Vec<Span<'static>>> {
    let styled = |content: String| -> Span<'static> {
        let style = spans
            .first()
            .map(|s| s.style)
            .unwrap_or_default()
            .patch(if head {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            });
        Span::styled(content, style)
    };
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        for word in span.content.split_inclusive(' ') {
            let w = word.width();
            if used > 0 && used + w > width {
                lines.push(Vec::new());
                used = 0;
            }
            if w <= width {
                let text = if used == 0 { word.trim_start() } else { word };
                let mut s = span.clone();
                s.content = text.to_owned().into();
                s.style = span.style.patch(if head {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                });
                if !s.content.is_empty() {
                    used += s.content.width();
                    lines.last_mut().expect("a cell always has a line").push(s);
                }
                continue;
            }
            // A word wider than the column: split it across the lines it needs.
            let mut rest: String = word.to_owned();
            while !rest.is_empty() {
                let space = width.saturating_sub(used);
                if space == 0 {
                    lines.push(Vec::new());
                    used = 0;
                    continue;
                }
                let mut take = space.min(rest.len());
                while take > 0 && !rest.is_char_boundary(take) {
                    take -= 1;
                }
                if take == 0 {
                    take = rest.len().min(1);
                }
                let (head_part, tail) = rest.split_at(take);
                // At the start of a line the split's own space is dropped, or a continuation would
                // begin with a blank and every wrapped row would look indented by one.
                let text = if used == 0 {
                    head_part.trim_start()
                } else {
                    head_part
                };
                if !text.is_empty() {
                    let mut s = span.clone();
                    s.content = text.to_owned().into();
                    s.style = span.style.patch(if head {
                        Style::new().add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    });
                    used += s.content.width();
                    lines.last_mut().expect("a cell always has a line").push(s);
                }
                rest = tail.to_owned();
            }
        }
    }
    if lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let _ = styled;
    lines
}

fn dim(theme: &Theme) -> Style {
    Style::new().fg(theme.muted)
}

/// Toggles that change how entries render.
#[derive(Clone, Copy)]
struct View {
    expand: bool,
    thinking: bool,
    spinner: usize,
    /// The transcript width, so an image's reserved rows match what is drawn (D59).
    room: (u16, u16),
    /// Whether the terminal can draw a picture, which decides whether an attachment gets rows
    /// reserved for one (D59).
    images: bool,
}

fn thinking_rows(text: &str, shown: bool, theme: &Theme) -> Vec<Row> {
    let text = text.trim();
    let bar = Style::new().fg(theme.thinking_text);
    if !shown {
        let n = text.lines().count();
        let plural = if n == 1 { "" } else { "s" };
        return vec![Row::plain(vec![Span::styled(
            format!("∴ thought for {n} line{plural} · ctrl+t shows it"),
            dim(theme).add_modifier(Modifier::ITALIC),
        )])];
    }
    text.lines()
        .map(|l| {
            Row::prefixed(
                "│ ",
                "│ ",
                bar,
                vec![Span::styled(
                    l.to_owned(),
                    dim(theme).add_modifier(Modifier::ITALIC),
                )],
            )
        })
        .collect()
}

/// What the user typed, read as markdown in their own colour (D81).
///
/// This is what pi does — its user-message component is its markdown renderer with the user's
/// colour as the base and the user-message background behind it — so a prompt and a reply are read
/// by the same code. The text is not bold, so that `**bold**` inside it is visible; the `>` marker is
/// what says whose message it is. Every row gets the marker's width as its continuation, including
/// a row a block construct made, so a list in a prompt still lines up under the message.
fn user_rows(text: &str, theme: &Theme, room: usize) -> Vec<Row> {
    let mut rows = markdown_over(text, theme, Style::new().fg(theme.user_message_text), room);
    let marker = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    for (i, row) in rows.iter_mut().enumerate() {
        row.first = vec![Span::styled(
            if i == 0 { "› " } else { "  " }.to_owned(),
            marker,
        )];
        row.rest = vec![Span::styled("  ".to_owned(), marker)];
    }
    rows
}

/// Rows for entries drawn as plain text; tool calls are drawn as blocks by [`tool_block`].
fn entry_rows(entry: &Entry, view: View, theme: &Theme) -> Vec<Row> {
    match entry {
        Entry::Thinking(text) => thinking_rows(text, view.thinking, theme),
        Entry::User(text) => user_rows(text, theme, view.room.0 as usize),
        Entry::Assistant(text) => markdown(text, theme, view.room.0 as usize),
        Entry::Image(image) => {
            // The picture is drawn over the rows after this one, so they are left empty for it. The
            // marker line stays, and is all a terminal without a graphics protocol shows (D59).
            let mut out: Vec<Row> = vec![Row::plain(vec![Span::styled(
                crate::tui::image::fallback(&image.name, image.width, image.height),
                dim(theme),
            )])];
            if view.images {
                // The rows the picture will cover, from its own size scaled into the transcript
                // width: kitty draws it at its pixel size when it fits, and scaled when it does
                // not (D59).
                let fit = crate::tui::image::fit(
                    (image.width, image.height),
                    (view.room.0, crate::tui::image::MAX_ROWS),
                );
                out.extend(std::iter::repeat_n(
                    Row::plain(Vec::new()),
                    fit.rows as usize,
                ));
            }
            out
        }
        Entry::Note(kind, text) => {
            let style = match kind {
                NoteKind::Info => dim(theme),
                NoteKind::Warn => Style::new().fg(theme.warning),
                NoteKind::Error => Style::new().fg(theme.error),
            };
            text.lines()
                .map(|l| Row::plain(vec![Span::styled(l.to_owned(), style)]))
                .collect()
        }
        Entry::Tool { .. } | Entry::Compaction(_) => Vec::new(),
    }
}

/// A compaction as a shaded block: what is being summarized, then the model's reasoning (unless
/// hidden with ctrl+t) and the summary as they stream in, then what was kept.
fn compaction_block(
    v: &CompactionView,
    thinking: bool,
    spinner: usize,
    theme: &Theme,
    room: usize,
) -> (Vec<Row>, Color) {
    let accent = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let muted = Style::new().fg(theme.tool_output);
    let italic = muted.add_modifier(Modifier::ITALIC);
    let (lead, bg) = match &v.state {
        CompactState::Running => (
            format!("{} ", SPINNER[spinner % SPINNER.len()]),
            theme.tool_pending_bg,
        ),
        CompactState::Done(_) => ("✻ ".to_owned(), theme.compact_done_bg),
        CompactState::Failed(_) => ("✻ ".to_owned(), theme.tool_error_bg),
    };
    let mut rows = vec![Row::plain(vec![
        Span::styled(lead, Style::new().fg(theme.warning)),
        Span::styled("compaction", accent),
        Span::styled(format!("  {}", v.header), muted),
    ])];
    let reasoning = v.reasoning.trim();
    if !reasoning.is_empty() {
        rows.push(Row::default());
        if thinking {
            rows.extend(reasoning.lines().map(|l| {
                Row::prefixed("│ ", "│ ", muted, vec![Span::styled(l.to_owned(), italic)])
            }));
        } else {
            rows.push(Row::plain(vec![Span::styled(
                format!(
                    "∴ reasoned for {} lines · ctrl+t shows it",
                    reasoning.lines().count()
                ),
                italic,
            )]));
        }
    }
    if !v.summary.trim().is_empty() {
        rows.push(Row::default());
        rows.extend(markdown(&v.summary, theme, room));
    }
    match &v.state {
        CompactState::Done(line) if !line.is_empty() => {
            rows.push(Row::default());
            rows.push(Row::plain(vec![Span::styled(line.clone(), italic)]));
        }
        CompactState::Failed(why) => {
            rows.push(Row::default());
            rows.push(Row::plain(vec![Span::styled(
                format!("compaction failed: {why}"),
                Style::new().fg(theme.tool_error_text),
            )]));
        }
        _ => {}
    }
    (rows, bg)
}

/// A tool's output as the block shows it. For bash, the exit-code line moves to the end (where
/// it stays visible under a long output) and the stream markers are dropped.
fn tool_output(name: &str, content: &str) -> (Vec<String>, Option<String>) {
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    if name != "bash" {
        return (lines, None);
    }
    let status = match lines.first().map(String::as_str) {
        Some("exit code 0") => {
            lines.remove(0);
            None
        }
        Some(first)
            if first.starts_with("exit code")
                || first.starts_with("timed out")
                || first.starts_with("killed by")
                || first.starts_with("cancelled") =>
        {
            Some(lines.remove(0))
        }
        _ => None,
    };
    lines.retain(|l| l != "--- stdout ---" && l != "--- stderr ---");
    (lines, status)
}

/// A tool call as a shaded block: the call, then its output. Collapsed, bash shows its last
/// lines (where errors and test summaries are) and other tools their first.
fn tool_block(
    name: &str,
    detail: &str,
    result: Option<&(bool, String)>,
    diff: Option<&str>,
    expand: bool,
    spinner: usize,
    theme: &Theme,
) -> (Vec<Row>, Color) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    // The tool's own name has a role of its own, so a theme can quiet the titles without touching
    // the accent everywhere else (D72).
    let accent = Style::new()
        .fg(theme.tool_title)
        .add_modifier(Modifier::BOLD);
    let mut detail_lines = detail.lines();
    let first = detail_lines.next().unwrap_or_default().to_owned();
    let mut header = Vec::new();
    if result.is_none() {
        header.push(Span::styled(
            format!("{} ", SPINNER[spinner % SPINNER.len()]),
            Style::new().fg(theme.warning),
        ));
    }
    if name == "bash" {
        header.push(Span::styled("$ ", accent));
        header.push(Span::styled(first, bold));
    } else {
        header.push(Span::styled(name.to_owned(), accent));
        header.push(Span::raw(" "));
        header.push(Span::styled(first, bold));
    }
    let mut rows = vec![Row::plain(header)];
    let more: Vec<&str> = detail_lines.collect();
    if expand {
        rows.extend(
            more.iter()
                .map(|l| Row::plain(vec![Span::styled((*l).to_owned(), bold)])),
        );
    } else if !more.is_empty() {
        rows[0].spans.push(Span::styled(" …", dim(theme)));
    }
    let Some((ok, content)) = result else {
        return (rows, theme.tool_pending_bg);
    };
    let (bg, text) = if *ok {
        (theme.tool_success_bg, Style::new().fg(theme.tool_output))
    } else {
        (theme.tool_error_bg, Style::new().fg(theme.tool_error_text))
    };
    let (lines, status) = tool_output(name, content);
    let note = Style::new()
        .fg(theme.tool_output)
        .add_modifier(Modifier::ITALIC);
    let mut body: Vec<Row> = Vec::new();
    if let (Some(diff), true) = (diff, *ok) {
        body = diff_rows(diff, expand, theme);
    } else if name == "read" && *ok && !expand {
        // A file's contents are noise in the transcript until asked for.
        body.push(Row::plain(vec![Span::styled(
            format!("{} lines", lines.len()),
            note,
        )]));
    } else if expand || lines.len() <= PREVIEW_LINES {
        body.extend(
            lines
                .iter()
                .map(|l| Row::plain(vec![Span::styled(l.clone(), text)])),
        );
    } else if name == "bash" {
        let hidden = lines.len() - PREVIEW_LINES;
        body.push(Row::plain(vec![Span::styled(
            format!("… {hidden} earlier lines · ctrl+o shows all"),
            note,
        )]));
        body.extend(
            lines[hidden..]
                .iter()
                .map(|l| Row::plain(vec![Span::styled(l.clone(), text)])),
        );
    } else {
        body.extend(
            lines[..PREVIEW_LINES]
                .iter()
                .map(|l| Row::plain(vec![Span::styled(l.clone(), text)])),
        );
        body.push(Row::plain(vec![Span::styled(
            format!(
                "… {} more lines · ctrl+o shows all",
                lines.len() - PREVIEW_LINES
            ),
            note,
        )]));
    }
    if let Some(status) = status {
        body.push(Row::plain(vec![Span::styled(
            status,
            Style::new()
                .fg(theme.tool_error_text)
                .add_modifier(Modifier::BOLD),
        )]));
    }
    if !body.is_empty() {
        rows.push(Row::default());
        rows.extend(body);
    }
    (rows, bg)
}

/// Diff lines shown in an edit or write block, before ctrl+o.
const DIFF_PREVIEW: usize = 16;

/// A diff from [`crate::tools`], one row per line: a gutter with the sign and line number, then
/// the text. Wrapped text stays to the right of the gutter.
fn diff_rows(diff: &str, expand: bool, theme: &Theme) -> Vec<Row> {
    let lines: Vec<&str> = diff.lines().collect();
    let width = lines
        .iter()
        .filter_map(|l| l.get(1..)?.split(' ').next())
        .map(str::len)
        .max()
        .unwrap_or(1);
    let shown = if expand {
        lines.len()
    } else {
        lines.len().min(DIFF_PREVIEW)
    };
    let gutter = Style::new().fg(theme.tool_diff_context);
    let mut rows = Vec::new();
    for line in &lines[..shown] {
        let (tag, rest) = line.split_at(line.len().min(1));
        if tag == "~" {
            rows.push(Row::plain(vec![Span::styled(
                format!("{:>width$}   ⋯", ""),
                gutter,
            )]));
            continue;
        }
        let (number, text) = rest.split_once(' ').unwrap_or((rest, ""));
        let (sign, style) = match tag {
            "-" => ("-", Style::new().fg(theme.tool_diff_removed)),
            "+" => ("+", Style::new().fg(theme.tool_diff_added)),
            _ => (" ", Style::new().fg(theme.tool_output)),
        };
        rows.push(Row {
            first: vec![
                Span::styled(sign.to_owned(), style),
                Span::styled(format!("{number:>width$} │ "), gutter),
            ],
            rest: vec![Span::styled(format!(" {:>width$} │ ", ""), gutter)],
            spans: vec![Span::styled(text.to_owned(), style)],
        });
    }
    if shown < lines.len() {
        rows.push(Row::plain(vec![Span::styled(
            format!("… {} more lines · ctrl+o shows all", lines.len() - shown),
            Style::new()
                .fg(theme.tool_output)
                .add_modifier(Modifier::ITALIC),
        )]));
    }
    rows
}

/// Rows on a background that spans the full width, with a column of padding on each side and a
/// line of padding above and below.
fn shaded(rows: &[Row], bg: Color, width: usize) -> Vec<Line<'static>> {
    let fill = Style::new().bg(bg);
    let blank = || Line::from(Span::styled(" ".repeat(width), fill));
    let inner = width.saturating_sub(2).max(1);
    let mut out = vec![blank()];
    for row in rows {
        for line in wrap(row, inner) {
            let used = spans_width(&line.spans);
            let mut spans = vec![Span::styled(" ", fill)];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.bg(bg))),
            );
            spans.push(Span::styled(
                " ".repeat(width.saturating_sub(used + 1)),
                fill,
            ));
            out.push(Line::from(spans));
        }
    }
    out.push(blank());
    out
}

/// All transcript lines at `width`, reusing cached entries.
/// The transcript, and the line each attachment is announced on, so the picture can be drawn just
/// below it (D59).
fn transcript_with_images(
    app: &mut App,
    width: u16,
) -> (
    Vec<Line<'static>>,
    Vec<(usize, crate::provider::Image, u32)>,
) {
    // The third field is the terminal id: it comes from the entry's position, so the same picture
    // keeps the same id every frame and does not have to be re-sent (D59).
    let mut images: Vec<(usize, crate::provider::Image, u32)> = Vec::new();
    let mut lines = Vec::new();
    for i in 0..app.entries.len() {
        let running = match &app.entries[i] {
            Entry::Tool { result, .. } => result.is_none(),
            Entry::Compaction(v) => v.state == CompactState::Running,
            _ => false,
        };
        let fresh = app.cache[i].as_ref().is_some_and(|c| {
            c.width == width && c.expand == app.expand_tools && c.thinking == app.show_thinking
        });
        if !fresh || running {
            let view = View {
                expand: app.expand_tools,
                thinking: app.show_thinking,
                spinner: app.spinner,
                room: (width, crate::tui::image::MAX_ROWS),
                images: app.protocol != crate::tui::image::Protocol::Text,
            };
            let rendered: Vec<Line<'static>> = match &app.entries[i] {
                Entry::Tool {
                    name,
                    detail,
                    result,
                    diff,
                } => {
                    let (rows, bg) = tool_block(
                        name,
                        detail,
                        result.as_ref(),
                        diff.as_deref(),
                        view.expand,
                        view.spinner,
                        &app.theme,
                    );
                    shaded(&rows, bg, width as usize)
                }
                Entry::Compaction(v) => {
                    let (rows, bg) = compaction_block(
                        v,
                        view.thinking,
                        view.spinner,
                        &app.theme,
                        width as usize,
                    );
                    shaded(&rows, bg, width as usize)
                }
                entry => entry_rows(entry, view, &app.theme)
                    .iter()
                    .flat_map(|r| wrap(r, width as usize))
                    .collect(),
            };
            app.cache[i] = Some(Cached {
                width,
                expand: app.expand_tools,
                thinking: app.show_thinking,
                lines: rendered,
            });
        }
        if i > 0 {
            lines.push(Line::default());
        }
        // The marker line is where the picture goes, so its index is taken before the entry's own
        // lines are added and the picture starts there.
        if let Some(image) = app.cached_image(i) {
            images.push((lines.len(), image, i as u32 + 1));
        }
        lines.extend(
            app.cache[i]
                .as_ref()
                .map(|c| c.lines.iter().cloned())
                .into_iter()
                .flatten(),
        );
    }
    (lines, images)
}

/// A rendered line's text, as it appears on screen.
fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The background a selected character sits on. It replaces only the colours, so dim tool output
/// stays readable while selected, and reversed text keeps its emphasis (D35).
fn selected_style(theme: &Theme) -> Style {
    Style::new().bg(theme.selected_bg).fg(theme.selection_fg)
}

/// Puts the selection's background behind characters `from..to` of one rendered line, splitting
/// spans where the selection starts and ends inside them.
fn shade(line: &mut Line<'static>, from: usize, to: usize, theme: &Theme) {
    if to <= from {
        return;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut index = 0usize;
    for span in std::mem::take(&mut line.spans) {
        let text = span.content.into_owned();
        let mut run = String::new();
        let mut selected: Option<bool> = None;
        for c in text.chars() {
            let here = index >= from && index < to;
            index += 1;
            if selected != Some(here) {
                flush(&mut out, &mut run, selected, span.style, theme);
                selected = Some(here);
            }
            run.push(c);
        }
        flush(&mut out, &mut run, selected, span.style, theme);
    }
    line.spans = out;
}

fn flush(
    out: &mut Vec<Span<'static>>,
    run: &mut String,
    selected: Option<bool>,
    style: Style,
    theme: &Theme,
) {
    if run.is_empty() {
        return;
    }
    let text = std::mem::take(run);
    out.push(match selected {
        Some(true) => Span::styled(text, style.patch(selected_style(theme))),
        _ => Span::styled(text, style),
    });
}

pub(super) fn home_relative(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(2) as usize;
    let layout = app.editor.layout(inner_width.max(1));
    let editor_rows = layout.rows.len().clamp(1, MAX_EDITOR_ROWS);
    let queue_rows = app.queue.len().min(3);
    let [
        transcript_area,
        queue_area,
        activity_area,
        editor_area,
        status_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(queue_rows as u16),
        Constraint::Length(1),
        Constraint::Length(editor_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_transcript(frame, app, transcript_area);

    let queued: Vec<Line> = app
        .queue
        .iter()
        .take(queue_rows)
        .map(|q| {
            let first = q.lines().next().unwrap_or_default();
            Line::from(vec![
                Span::styled("queued › ", dim(&app.theme)),
                Span::raw(first.to_owned()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(queued), queue_area);

    let activity = match (&app.busy, &app.stopped) {
        (Some(activity), _) => {
            let spin = SPINNER[app.spinner % SPINNER.len()];
            let what = if app.cancelling {
                "cancelling…".to_owned()
            } else {
                match activity {
                    Activity::Waiting => format!("waiting for {}", app.header.model),
                    Activity::Thinking => "thinking".to_owned(),
                    Activity::Compacting => "compacting the conversation".to_owned(),
                    Activity::Writing => "writing".to_owned(),
                    Activity::Tool(name) => format!("running {name}"),
                }
            };
            Line::from(vec![
                Span::styled(format!("{spin} "), Style::new().fg(app.theme.warning)),
                Span::raw(what),
                Span::styled("  esc to cancel", dim(&app.theme)),
            ])
        }
        (None, Some(reason)) => Line::from(Span::styled(
            format!("session stopped: {reason}"),
            Style::new().fg(app.theme.error),
        )),
        (None, None) => match app.copied.filter(|at| at.elapsed() < COPY_NOTE) {
            Some(_) => {
                let mut spans = vec![Span::styled(
                    format!("copied {}", app.copied_what),
                    Style::new().fg(app.theme.success),
                )];
                // Only the transcript's own selection needs the hint; a dialog's text was never
                // the terminal's to select (D86).
                if app.copied_what == "the selection" {
                    spans.push(Span::styled(
                        "  (shift+drag still selects in the terminal)",
                        dim(&app.theme),
                    ));
                }
                Line::from(spans)
            }
            None => Line::from(Span::styled(
                "enter send · shift+enter newline · /help",
                dim(&app.theme),
            )),
        },
    };
    frame.render_widget(Paragraph::new(activity), activity_area);

    // The editor scrolls to keep the cursor visible.
    let (cursor_row, cursor_col) = layout.cursor;
    let top = (cursor_row + 1).saturating_sub(editor_rows);
    let shown: Vec<Line> = layout.rows[top..(top + editor_rows).min(layout.rows.len())]
        .iter()
        .map(|r| Line::raw(r.clone()))
        .collect();
    // The editor's border is the one place that says whether the agent is working: muted while a
    // turn runs, the accent colour when it is waiting for you.
    // Three states, three roles: a command being typed, a turn running, and waiting for you (D72).
    let border = if app.editor.text().trim_start().starts_with('!') {
        app.theme.bash_mode
    } else if app.busy.is_some() {
        app.theme.border_muted
    } else {
        app.theme.border_accent
    };
    let editor = Paragraph::new(shown).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border)),
    );
    frame.render_widget(editor, editor_area);
    frame.set_cursor_position(Position::new(
        editor_area.x + 1 + cursor_col as u16,
        editor_area.y + 1 + (cursor_row - top) as u16,
    ));

    // The line is whatever `[ui] status` says, filled from the session (D36).
    let right = app.header.status.render(&StatusData {
        provider: &app.header.provider,
        model: &app.header.model,
        usage: app.usage,
        cost: app.cost,
        priced: app.header.priced,
        quota: app.quota.as_ref(),
        context: app.context,
        window: app.header.context_window,
        workspace: &home_relative(&app.header.workspace),
        // A refill is counted down from the moment the frame is drawn (D49).
        now: std::time::SystemTime::now(),
    });
    // The workspace is drawn on the left unless the template already places it, so a user who
    // moves it into the line does not see it twice (D36).
    if !app
        .header
        .status
        .contains(crate::tui::status::Field::Workspace)
    {
        let room = (status_area.width as usize).saturating_sub(right.width() + 2);
        let left = keep_tail(&home_relative(&app.header.workspace), room);
        frame.render_widget(
            Paragraph::new(left).style(Style::new().fg(app.theme.muted)),
            status_area,
        );
    }
    frame.render_widget(
        Paragraph::new(right)
            .style(Style::new().fg(app.theme.muted))
            .alignment(Alignment::Right),
        status_area,
    );

    draw_menu(frame, app, editor_area);

    // A dialog owns the screen while it is open, and its text is selected and copied the way the
    // transcript's is, so the text it drew is kept for the mouse (D87).
    let mut surface = None;
    if let Some(picker) = &app.resume {
        surface = Some(draw_resume(frame, picker, area, &app.theme));
    }
    if let Some(tree) = &app.tree {
        surface = Some(draw_tree(frame, tree, area, &app.theme));
    }
    if let Some(dialog) = &app.settings {
        surface = Some(draw_settings(frame, dialog, area, &app.theme));
    }
    if let Some(dialog) = &app.permissions {
        surface = Some(draw_permissions(frame, dialog, area, &app.theme));
    }
    if let Some(picker) = &app.themes {
        // Drawn in the theme the highlight offers, which is the point of the preview (D74).
        surface = Some(draw_themes(frame, picker, area, &app.theme));
    }
    app.open_url_at = None;
    if let Some(login) = &app.login {
        let (seen, link) = draw_login(frame, login, area, app.spinner, &app.theme);
        // Kept for the click: a link drawn on the screen is not the terminal's to click (D86).
        app.open_url_at = link;
        surface = Some(seen);
    }
    let had_dialog = app.dialog_seen.is_some();
    let opened = !had_dialog && surface.is_some();
    app.dialog_seen = surface;
    if opened || (had_dialog && app.dialog_seen.is_none()) {
        // The selection belonged to the surface that has just changed, and its line numbers mean
        // nothing on the other one: keeping it would highlight unrelated text and copy it (D87).
        app.selection = None;
    }
    shade_dialog(frame, app);
}

/// Puts the selection's background behind the dialog's cells, on the buffer itself (D87).
///
/// The transcript's selection is shaded by splitting spans before they are drawn. A dialog is built
/// by a dozen functions that each own their lines, so its selection is painted where it ended up
/// instead: the cells of the box the drag covered, which is the same thing from the reader's side.
fn shade_dialog(frame: &mut Frame<'_>, app: &App) {
    let (Some(seen), Some(sel)) = (app.dialog_seen.as_ref(), app.selection) else {
        return;
    };
    if sel.is_empty() {
        return;
    }
    let (first, last) = sel.ends();
    let buf = frame.buffer_mut();
    let height = seen.lines.len() as u16;
    for line in first.line..=last.line.min(seen.lines.len().saturating_sub(1)) {
        let row = seen.area.y.saturating_add(line as u16);
        if line as u16 >= height || row >= seen.area.bottom() {
            break;
        }
        let chars = seen.lines[line].chars().count();
        let from = if line == first.line { first.ch } else { 0 };
        let to = if line == last.line {
            last.ch.min(chars)
        } else {
            chars
        };
        for ch in from..to {
            let col = seen.area.x.saturating_add(ch as u16);
            if col >= seen.area.right() {
                break;
            }
            let cell = &mut buf[(col, row)];
            cell.set_style(cell.style().patch(selected_style(&app.theme)));
        }
    }
}

/// The `/` menu, drawn over the transcript just above the editor.
fn draw_menu(frame: &mut Frame<'_>, app: &mut App, editor_area: Rect) {
    const ROWS: usize = 8;
    let Some(popup) = app.popup() else {
        return;
    };
    let selected = popup.selected;
    let top = (selected + 1).saturating_sub(ROWS);
    let label_width = popup
        .items
        .iter()
        .map(|(label, _)| label.width())
        .max()
        .unwrap_or(0);
    let accent = Style::new().fg(app.theme.accent);
    let room = (editor_area.width as usize).saturating_sub(4);
    let lines: Vec<Line> = popup
        .items
        .iter()
        .enumerate()
        .skip(top)
        .take(ROWS)
        .map(|(i, (label, about))| {
            let chosen = i == selected;
            let label = keep_tail(label, room);
            let mut spans = vec![
                Span::styled(if chosen { "› " } else { "  " }, accent),
                Span::styled(
                    format!("{label:<label_width$}"),
                    if chosen {
                        accent.add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ),
            ];
            if !about.is_empty() {
                spans.push(Span::styled(format!("  {about}"), dim(&app.theme)));
            }
            Line::from(spans)
        })
        .collect();
    let height = lines.len() as u16 + 2;
    let rect = Rect::new(
        editor_area.x,
        editor_area.y.saturating_sub(height),
        editor_area.width,
        height.min(editor_area.y),
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(dim(&app.theme))
                .title_bottom(Span::styled(popup.hint, dim(&app.theme))),
        ),
        rect,
    );
}

fn login_title(login: &Login) -> String {
    let c = login.choice();
    if c.custom {
        return " Add an OpenAI-compatible server ".to_owned();
    }
    // The picker names the model in force, so the list is read against something (D88).
    match (login.purpose(), c.model.as_deref()) {
        (crate::tui::login::Purpose::Model, Some(model)) if !model.is_empty() => {
            format!(" Model for {} · {model} ", c.name)
        }
        (crate::tui::login::Purpose::Model, _) => format!(" Model for {} ", c.name),
        _ => format!(" Log in to {} ", c.name),
    }
}

/// The /login dialog, centred over the screen. Keys are shown as dots.
/// Draws the login dialog, and answers with where the link was drawn, so a click on it can open it
/// rather than doing nothing (D86).
fn draw_login(
    frame: &mut Frame<'_>,
    login: &Login,
    area: Rect,
    spinner: usize,
    theme: &Theme,
) -> (Seen, Option<Rect>) {
    let mut link_at = None;
    let width = area.width.saturating_sub(4).min(76);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    let title = match &login.stage {
        Stage::Pick => {
            lines.push(Line::from("Choose a provider".bold()));
            lines.push(Line::default());
            let name_width = login
                .choices
                .iter()
                .filter(|c| !c.custom)
                .map(|c| c.name.width())
                .max()
                .unwrap_or(0);
            for (i, c) in login.choices.iter().enumerate() {
                let chosen = i == login.selected;
                let mut spans = vec![
                    Span::styled(if chosen { "› " } else { "  " }, accent),
                    Span::styled(
                        format!("{:<name_width$}", c.name),
                        if chosen {
                            Style::new().add_modifier(Modifier::BOLD)
                        } else {
                            Style::new()
                        },
                    ),
                ];
                if !c.detail.is_empty() {
                    spans.push(Span::styled(format!("  {}", c.detail), dim(theme)));
                }
                if !c.status.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", c.status),
                        Style::new().fg(theme.success),
                    ));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "↑↓ choose · enter select · esc close",
                dim(theme),
            )));
            " Log in ".to_owned()
        }
        Stage::Fill => {
            let (fields, at) = (&login.fields, login.at);
            for (i, f) in fields.iter().enumerate().take(at + 1) {
                let shown = if f.masked() {
                    "•".repeat(f.value.chars().count())
                } else {
                    f.value.clone()
                };
                if i < at {
                    let shown = if shown.is_empty() {
                        "(none)".into()
                    } else {
                        shown
                    };
                    let label = format!("{}: ", f.label);
                    let room = inner.saturating_sub(label.width());
                    lines.push(Line::from(vec![
                        Span::styled(label, dim(theme)),
                        Span::raw(keep_tail(&shown, room)),
                    ]));
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    f.label,
                    Style::new().add_modifier(Modifier::BOLD),
                )));
                let visible = keep_tail(&shown, inner.saturating_sub(3));
                cursor = Some((lines.len(), 2 + visible.width()));
                lines.push(Line::from(vec![
                    Span::styled("› ", accent),
                    Span::raw(visible),
                ]));
                if f.masked() {
                    if let Some(page) = crate::provider::key_page(&login.choice().name) {
                        lines.push(Line::from(Span::styled(
                            format!("get one at {page}"),
                            dim(theme),
                        )));
                    }
                    let note = format!("stored in {}, readable only by you", login.auth_path);
                    lines.push(Line::from(Span::styled(
                        keep_tail(&note, inner),
                        dim(theme),
                    )));
                }
            }
            if let Some(e) = &login.error {
                lines.push(Line::from(Span::styled(
                    keep_tail(e, inner),
                    Style::new().fg(theme.error),
                )));
            }
            lines.push(Line::default());
            let esc = if login.esc_closes() {
                "esc close"
            } else {
                "esc back"
            };
            lines.push(Line::from(Span::styled(
                format!("enter next · {esc}"),
                dim(theme),
            )));
            login_title(login)
        }
        Stage::Checking { .. } => {
            let picker = login.purpose() == crate::tui::login::Purpose::Model;
            let spin = SPINNER[spinner % SPINNER.len()];
            lines.push(Line::from(vec![
                Span::styled(format!("{spin} "), Style::new().fg(theme.warning)),
                Span::raw(if picker {
                    "listing the models this provider serves…"
                } else {
                    "checking the key and listing models…"
                }),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                if picker { "esc close" } else { "esc back" },
                dim(theme),
            )));
            login_title(login)
        }
        Stage::SigningIn { .. } => {
            let d = &login.device;
            // The failure goes beside the code rather than instead of it: what failed is usually
            // the browser, and the address and code are still what the person needs (D86).
            if let Some(code) = &d.code {
                lines.push(Line::from("A browser on any device signs this in".bold()));
                lines.push(Line::default());
                // The link is underlined so it reads as one, and its cells are recorded: a click
                // here opens the browser, because drawn text is not the terminal's to click (D86).
                lines.push(Line::from(vec![
                    Span::styled("  1. open  ", dim(theme)),
                    Span::styled(
                        d.url.clone(),
                        Style::new()
                            .fg(theme.accent)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]));
                // The column the address begins at, inside the box: the label it follows.
                let start = "  1. open  ".width();
                let room = inner.saturating_sub(start);
                // Only the part of the address that fits is on screen, so only that is clickable:
                // the box is drawn from the left and the rest never appears.
                let shown = d.url.width().min(room);
                link_at = Some((lines.len() - 1, start, shown));
                lines.push(Line::from(vec![
                    Span::styled("  2. enter ", dim(theme)),
                    Span::styled(
                        code.clone(),
                        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                ]));
                let spin = SPINNER[spinner % SPINNER.len()];
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::styled(format!("{spin} "), Style::new().fg(theme.warning)),
                    Span::raw("waiting for the sign-in…"),
                ]));
                lines.push(Line::from(Span::styled(
                    "the code is on your clipboard, ready to paste",
                    dim(theme),
                )));
            } else if let Some(why) = &d.error {
                for line in wrap_text(why, inner) {
                    lines.push(Line::from(Span::styled(line, Style::new().fg(theme.error))));
                }
            } else {
                let spin = SPINNER[spinner % SPINNER.len()];
                lines.push(Line::from(vec![
                    Span::styled(format!("{spin} "), Style::new().fg(theme.warning)),
                    Span::raw("asking for a sign-in code…"),
                ]));
            }
            if let Some(why) = &d.error
                && d.code.is_some()
            {
                lines.push(Line::default());
                for line in wrap_text(why, inner) {
                    lines.push(Line::from(Span::styled(line, Style::new().fg(theme.error))));
                }
            }
            lines.push(Line::default());
            let hint = if d.url.is_empty() {
                "esc cancel".to_owned()
            } else {
                "o open · c copy link · esc cancel".to_owned()
            };
            lines.push(Line::from(Span::styled(hint, dim(theme))));
            login_title(login)
        }
        Stage::Models {
            models,
            filter,
            selected,
            note,
        } => {
            lines.push(Line::from("Choose a model".bold()));
            if let Some(note) = note {
                lines.push(Line::from(Span::styled(
                    keep_tail(note, inner),
                    Style::new().fg(theme.warning),
                )));
            }
            let visible = keep_tail(filter, inner.saturating_sub(10));
            cursor = Some((lines.len(), 10 + visible.width()));
            lines.push(Line::from(vec![
                Span::styled("filter  › ", accent),
                Span::raw(visible),
            ]));
            let list = matching(models, filter);
            let top = (selected + 1).saturating_sub(MODEL_ROWS);
            for (i, m) in list.iter().enumerate().skip(top).take(MODEL_ROWS) {
                let chosen = i == *selected;
                lines.push(Line::from(vec![
                    Span::styled(if chosen { "› " } else { "  " }, accent),
                    Span::styled(
                        keep_tail(m, inner.saturating_sub(2)),
                        if chosen {
                            Style::new().add_modifier(Modifier::BOLD)
                        } else {
                            Style::new()
                        },
                    ),
                ]));
            }
            if list.is_empty() && !filter.trim().is_empty() {
                lines.push(Line::from(Span::styled(
                    keep_tail(
                        &format!("enter uses \"{}\" as the model name", filter.trim()),
                        inner,
                    ),
                    dim(theme),
                )));
            }
            lines.push(Line::default());
            let esc = if login.purpose() == crate::tui::login::Purpose::Model {
                "esc close"
            } else {
                "esc back"
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{} of {} · type to filter · enter choose · {esc}",
                    list.len(),
                    models.len()
                ),
                dim(theme),
            )));
            login_title(login)
        }
    };
    let at = modal(frame, area, width, &title, lines, cursor, theme);
    // A row inside the box, and a column inside the address that was drawn there.
    let link = link_at.map(|(row, col, len)| {
        let x = at.rect.x.saturating_add(1).saturating_add(col as u16);
        let room = at.rect.right().saturating_sub(1).saturating_sub(x);
        Rect::new(
            x,
            at.rect.y.saturating_add(1).saturating_add(row as u16),
            (len as u16).min(room),
            1,
        )
    });
    (at.seen, link)
}

/// A bordered box centred over `area`, with an optional cursor at (row, column) inside it.
fn modal(
    frame: &mut Frame<'_>,
    area: Rect,
    width: u16,
    title: &str,
    lines: Vec<Line<'_>>,
    cursor: Option<(usize, usize)>,
    theme: &Theme,
) -> Surface {
    let height = (lines.len() as u16 + 2).min(area.height);
    // What each row says, cut to the cells it occupies: a dialog clips its text at the border, so
    // the part that was never drawn is not selectable either (D87).
    let inner = width.saturating_sub(2) as usize;
    let text: Vec<String> = lines
        .iter()
        .map(|line| plain(line).chars().take(inner).collect())
        .collect();
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(theme.border))
                .title(title.to_owned()),
        ),
        rect,
    );
    if let Some((row, col)) = cursor {
        frame.set_cursor_position(Position::new(
            rect.x + 1 + col as u16,
            rect.y + 1 + row as u16,
        ));
    }
    Surface {
        seen: Seen {
            // The text, not the border: a drag that starts on the frame selects the line it is on.
            area: Rect::new(
                rect.x.saturating_add(1),
                rect.y.saturating_add(1),
                rect.width.saturating_sub(2),
                rect.height.saturating_sub(2),
            ),
            top: 0,
            lines: text,
        },
        rect,
    }
}

/// What a modal put on the screen: the box it drew in, and the text inside it (D87).
struct Surface {
    seen: Seen,
    rect: Rect,
}

/// `/tree`: every message on the branch, the one the conversation is at, and where another branch
/// leaves from (D54).
fn draw_tree(frame: &mut Frame<'_>, tree: &Tree, area: Rect, theme: &Theme) -> Seen {
    let width = area.width.saturating_sub(4).min(96);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let mut lines: Vec<Line> = Vec::new();
    let rows = tree.visible();
    let top = tree.offset();
    for (i, row) in rows.iter().enumerate().skip(top).take(TREE_ROWS) {
        let chosen = i == tree.selected;
        let label = keep_head(&tree.label(row), inner.saturating_sub(2));
        // The message the conversation is at is marked in green, so "where am I" is answered
        // before reading a word; a message on a branch the next request would not carry is dimmed
        // (D54).
        let style = if chosen {
            Style::new().add_modifier(Modifier::BOLD)
        } else if tree.is_here(row) {
            Style::new().fg(theme.success)
        } else if tree.is_other(row) {
            dim(theme)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(if chosen { "› " } else { "  " }, accent),
            Span::styled(label, style),
        ]));
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet", dim(theme))));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(
            "{} prompts{} · enter continue here · ctrl+u prompts only · ctrl+x copy · esc closes",
            tree.prompts(),
            if tree.user_only { " (filtered)" } else { "" }
        ),
        dim(theme),
    )));
    modal(
        frame,
        area,
        width,
        " Conversation tree ",
        lines,
        None,
        theme,
    )
    .seen
}

fn draw_resume(frame: &mut Frame<'_>, picker: &SessionPicker, area: Rect, theme: &Theme) -> Seen {
    let width = area.width.saturating_sub(4).min(96);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let now = std::time::SystemTime::now();
    let mut lines: Vec<Line> = Vec::new();
    let visible_filter = keep_tail(&picker.filter, inner.saturating_sub(10));
    let cursor = Some((0, 10 + visible_filter.width()));
    lines.push(Line::from(vec![
        Span::styled("filter  › ", accent),
        Span::raw(visible_filter),
    ]));
    let shown = picker.visible();
    let top = (picker.selected + 1).saturating_sub(SESSION_ROWS);
    for (i, s) in shown.iter().enumerate().skip(top).take(SESSION_ROWS) {
        let chosen = i == picker.selected;
        let plural = if s.prompts == 1 { "" } else { "s" };
        let meta = format!("  {} · {} prompt{plural}", age(s.updated, now), s.prompts);
        // A session that came from a fork says so, so two rows with the same title are told apart
        // (D53).
        let head = if s.forked_from.is_some() {
            format!("↳ {}", s.title)
        } else {
            s.title.clone()
        };
        let room = inner.saturating_sub(2 + meta.width());
        let title = keep_head(&head, room);
        let pad = room.saturating_sub(title.width());
        lines.push(Line::from(vec![
            Span::styled(if chosen { "› " } else { "  " }, accent),
            Span::styled(
                title,
                if chosen {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(meta, dim(theme)),
        ]));
    }
    if shown.is_empty() {
        lines.push(Line::from(Span::styled("no session matches", dim(theme))));
    }
    lines.push(Line::default());
    // A delete needs the key twice, so the footer says so while it is waiting for the second one.
    let confirming = picker
        .confirming
        .as_ref()
        .and_then(|id| picker.sessions.iter().find(|s| &s.id == id))
        .map(|s| s.title.clone());
    let help = match &confirming {
        Some(title) => format!("ctrl+d again deletes `{title}` · any other key keeps it"),
        None => format!(
            "{} of {} · type to filter · enter resume · ctrl+r rename · ctrl+d delete · esc close",
            shown.len(),
            picker.sessions.len()
        ),
    };
    lines.push(Line::from(Span::styled(
        help,
        if confirming.is_some() {
            Style::new().fg(theme.error)
        } else {
            dim(theme)
        },
    )));
    modal(
        frame,
        area,
        width,
        " Resume a session ",
        lines,
        cursor,
        theme,
    )
    .seen
}

fn draw_settings(
    frame: &mut Frame<'_>,
    dialog: &SettingsDialog,
    area: Rect,
    theme: &Theme,
) -> Seen {
    let width = area.width.saturating_sub(4).min(84);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let label_width = ROWS.iter().map(|r| r.label().width()).max().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    for (i, setting) in ROWS.iter().enumerate() {
        let chosen = i == dialog.selected;
        let label = format!("{:<label_width$}  ", setting.label());
        let room = inner.saturating_sub(2 + label.width());
        let value = match (&dialog.editing, chosen) {
            (Some(text), true) => {
                cursor = Some((lines.len(), 2 + label.width() + text.width()));
                Span::styled(
                    text.clone(),
                    Style::new().add_modifier(Modifier::UNDERLINED),
                )
            }
            _ => Span::styled(keep_tail(&dialog.value(*setting), room), accent),
        };
        lines.push(Line::from(vec![
            Span::styled(if chosen { "› " } else { "  " }, accent),
            Span::styled(
                label,
                if chosen {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            ),
            value,
        ]));
    }
    lines.push(Line::default());
    let help = match &dialog.error {
        Some(e) => Span::styled(keep_tail(e, inner), Style::new().fg(theme.error)),
        None => Span::styled(keep_tail(dialog.current().help(), inner), dim(theme)),
    };
    lines.push(Line::from(help));
    let keys = if dialog.editing.is_some() {
        "enter save · esc cancel"
    } else {
        "↑↓ choose · enter change · esc close"
    };
    lines.push(Line::from(Span::styled(keys, dim(theme))));
    modal(frame, area, width, " Settings ", lines, cursor, theme).seen
}

/// `/theme`: every theme by name, the highlight previewed on the whole screen (D74).
fn draw_themes(
    frame: &mut Frame<'_>,
    picker: &crate::tui::themes::ThemePicker,
    area: Rect,
    theme: &Theme,
) -> Seen {
    use crate::tui::themes::ROWS as THEME_ROWS;
    let width = area.width.saturating_sub(4).min(72);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let name_width = picker
        .entries
        .iter()
        .map(|e| e.name.width())
        .max()
        .unwrap_or(0)
        .min(inner / 2);
    let mut lines: Vec<Line> = Vec::new();
    let top = picker.offset();
    for (i, entry) in picker.entries.iter().enumerate().skip(top).take(THEME_ROWS) {
        let chosen = i == picker.selected;
        // The theme that was in force when the list opened, so escape has something to name.
        let was = if entry.name == picker.original {
            "*"
        } else {
            " "
        };
        let name = format!("{:<name_width$}", entry.name);
        let note = keep_tail(&entry.note(), inner.saturating_sub(3 + name_width + 12));
        let style = if entry.theme.is_ok() {
            Style::new()
        } else {
            Style::new().fg(theme.error)
        };
        lines.push(Line::from(vec![
            Span::styled(if chosen { "› " } else { "  " }, accent),
            Span::styled(
                format!("{was}{name}"),
                if chosen {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            ),
            Span::styled(
                format!("  {:<12}", entry.source.label()),
                if chosen { accent } else { dim(theme) },
            ),
            Span::styled(note, dim(theme)),
        ]));
    }
    lines.push(Line::default());
    // What the highlight does, in the theme it offers: the preview is the whole point.
    let help = match &picker.current().theme {
        Ok(_) => format!(
            "{} · {} · enter uses it and saves it",
            picker.current().name,
            picker.current().note()
        ),
        Err(e) => format!("{} · {e}", picker.current().name),
    };
    lines.push(Line::from(Span::styled(
        keep_tail(&help, inner),
        match &picker.current().theme {
            Ok(_) => dim(theme),
            Err(_) => Style::new().fg(theme.error),
        },
    )));
    lines.push(Line::from(Span::styled(
        "* was in force · ↑↓ preview · enter choose · esc back",
        dim(theme),
    )));
    modal(frame, area, width, " Theme ", lines, None, theme).seen
}

fn draw_permissions(
    frame: &mut Frame<'_>,
    dialog: &Permissions,
    area: Rect,
    theme: &Theme,
) -> Seen {
    let width = area.width.saturating_sub(4).min(88);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let rule_width = dialog
        .rows
        .iter()
        .map(|r| r.rule.width())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    for (i, row) in dialog.rows.iter().enumerate() {
        let chosen = i == dialog.selected;
        let mark = if row.editable { "" } else { "·" };
        let rule = format!("{:<rule_width$}{mark}", row.rule);
        let what = if row.kind == Kind::FsRead || row.kind == Kind::FsWrite {
            let mut text = format!("{} {}", row.kind.label(), row.root);
            if !row.exclude.is_empty() {
                text.push_str(&format!("  exclude [{}]", row.exclude));
            }
            text
        } else if row.kind == Kind::Net {
            format!("network {}", row.root)
        } else {
            format!("{} {}", row.kind.label(), row.root)
        };
        let value = match (&dialog.editing, chosen) {
            (Some(text), true) => {
                cursor = Some((lines.len(), 2 + rule.width() + 1 + text.width()));
                Span::styled(
                    text.clone(),
                    Style::new().add_modifier(Modifier::UNDERLINED),
                )
            }
            _ => {
                let room = inner.saturating_sub(2 + rule.width() + 1);
                Span::styled(
                    keep_head(&what, room),
                    if row.editable { accent } else { dim(theme) },
                )
            }
        };
        lines.push(Line::from(vec![
            Span::styled(if chosen { "› " } else { "  " }, accent),
            Span::styled(
                rule,
                if chosen {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            ),
            Span::raw(" "),
            value,
        ]));
    }
    if dialog.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "this project's policy grants nothing yet",
            dim(theme),
        )));
    }
    lines.push(Line::default());
    if let Some(typed) = &dialog.adding {
        // The prompt replaces the help line, so the forms are visible while typing.
        lines.push(Line::from(vec![
            Span::styled("add  › ", Style::new().fg(theme.accent)),
            Span::styled(
                typed.clone(),
                Style::new().add_modifier(Modifier::UNDERLINED),
            ),
        ]));
        cursor = Some((lines.len() - 1, 2 + 5 + typed.width()));
        lines.push(Line::from(Span::styled(
            keep_tail(ADD_HELP, inner),
            dim(theme),
        )));
    } else {
        let help = match &dialog.error {
            Some(e) => Span::styled(keep_tail(e, inner), Style::new().fg(theme.error)),
            None => Span::styled(keep_tail(&dialog.help(), inner), dim(theme)),
        };
        lines.push(Line::from(help));
    }
    lines.push(Line::from(Span::styled(
        format!(
            "{} · {}",
            keep_tail(&dialog.policy_path, inner / 2),
            keep_tail(&dialog.global_path, inner / 2)
        ),
        dim(theme),
    )));
    let keys = if dialog.editing.is_some() {
        "enter save · esc cancel"
    } else {
        "↑↓ choose · enter change · esc close"
    };
    lines.push(Line::from(Span::styled(keys, dim(theme))));
    modal(
        frame,
        area,
        width,
        " Permissions for this project ",
        lines,
        cursor,
        theme,
    )
    .seen
}

/// The start of `s` in at most `max` columns, with `…` marking the cut.
fn keep_head(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// The end of `s` in at most `max` columns, with `…` marking the cut.
/// A message as lines that fit `width`, so a failed click says all of what went wrong rather than
/// the tail of it (D86).
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.truncate(4);
    out
}

fn keep_tail(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    std::iter::once('…').chain(out.into_iter().rev()).collect()
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let (mut lines, at) = transcript_with_images(app, area.width.max(1));
    let mut placed: Vec<Placed> = Vec::new();
    let height = area.height as usize;
    let total = lines.len();
    // While scrolled up, new output must not move the view.
    if app.scroll > 0 && total > app.total_lines {
        app.scroll += total - app.total_lines;
    }
    app.scroll = app.scroll.min(total.saturating_sub(height));
    app.total_lines = total;
    app.view_height = height;
    let top = total.saturating_sub(height + app.scroll);
    // The text of each line as drawn: a click maps to a character through it, and a copy takes
    // exactly what the screen showed (D35).
    let text: Vec<String> = lines.iter().map(plain).collect();
    // Only when the transcript owns the selection: a drag inside an open dialog records line
    // numbers in the dialog's own frame, and shading these lines with them painted a second
    // highlight near the top of the screen (D87).
    // A dialog is a surface of its own: a drag inside it records line numbers in its own frame, and
    // shading these lines with them painted a second highlight near the top of the screen (D87).
    let dialog_open = app.resume.is_some()
        || app.tree.is_some()
        || app.settings.is_some()
        || app.permissions.is_some()
        || app.themes.is_some()
        || app.login.is_some();
    if let Some(sel) = app.selection
        && !sel.is_empty()
        && !dialog_open
    {
        let (first, last) = sel.ends();
        for (i, line) in lines.iter_mut().enumerate().skip(top).take(height) {
            if i < first.line || i > last.line {
                continue;
            }
            let from = if i == first.line { first.ch } else { 0 };
            let to = if i == last.line { last.ch } else { usize::MAX };
            shade(line, from, to, &app.theme);
        }
    }
    // An image is drawn in the rows below its marker line, and stays on screen while any of those
    // rows is: the marker scrolling off the top does not take the picture with it (D59).
    let room = area.width.saturating_sub(4).max(8);
    for (line, image, id) in at {
        let box_ = crate::tui::image::fit(
            (image.width, image.height),
            (room, crate::tui::image::MAX_ROWS),
        );
        if crate::tui::image::band(line, box_.rows, top, height).is_none() {
            continue;
        }
        placed.push(Placed {
            line,
            id,
            box_,
            pixels: (image.width, image.height),
            png: image.bytes().unwrap_or_default(),
        });
    }
    let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
    app.images = placed;
    app.seen = Some(Seen {
        area,
        top,
        lines: text,
    });
    app.jump_badge = None;
    if app.scroll > 0 {
        let plural = if app.scroll == 1 { "" } else { "s" };
        let full = format!(
            " ↓ {} more line{plural} · end or click jumps to the end ",
            app.scroll
        );
        let short = format!(" ↓ {} more · end ", app.scroll);
        let note = if full.width() <= area.width as usize {
            full
        } else {
            short
        };
        let w = (note.width() as u16).min(area.width);
        let r = Rect::new(
            area.right().saturating_sub(w),
            area.bottom().saturating_sub(1),
            w,
            1,
        );
        frame.render_widget(
            Paragraph::new(note).style(
                Style::new()
                    .fg(app.theme.selection_fg)
                    .bg(app.theme.warning),
            ),
            r,
        );
        app.jump_badge = Some(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::End;
    use crate::tui::app::{AgentEvent, Header};
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn img(name: &str, w: u32, h: u32) -> crate::provider::Image {
        crate::provider::Image {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".into(),
            name: name.into(),
            width: w,
            height: h,
        }
    }

    #[test]
    fn two_attached_pictures_are_both_placed() {
        // Two images in one conversation is the ordinary case for "@a.png @b.png" (D59).
        let mut app = bare_app();
        app.protocol = crate::tui::image::Protocol::Kitty;
        app.entries.push(Entry::User("look at these".into()));
        app.cache.push(None);
        app.entries.push(Entry::Image(img("one.png", 64, 64)));
        app.cache.push(None);
        app.entries.push(Entry::Image(img("two.png", 64, 64)));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(
            app.images.len(),
            2,
            "both pictures are placed: {:?}",
            app.images
        );
        assert_ne!(app.images[0].id, app.images[1].id, "with different ids");
        // And the drawing pass plans both, on their own rows.
        let area = ratatui::layout::Rect::new(0, 0, 80, 40);
        let (_, draw, _) = crate::tui::plan_images(
            crate::tui::image::Protocol::Kitty,
            &app.drawn,
            &app.images,
            area,
            0,
        );
        assert_eq!(draw.len(), 2, "{draw:?}");
        assert_ne!(draw[0].0, draw[1].0, "on different rows: {draw:?}");
    }

    #[test]
    fn a_picture_is_placed_while_any_of_it_is_on_screen() {
        // The bug: placement was skipped unless the picture's *marker line* was inside the view, so
        // scrolling the marker off the top made the picture disappear although nearly all of it was
        // still on screen. The marker is a caption, not the picture (D59).
        let mut app = bare_app();
        app.protocol = crate::tui::image::Protocol::Kitty;
        app.entries.push(Entry::User("look at this".into()));
        app.cache.push(None);
        app.entries.push(Entry::Image(img("shot.png", 64, 64)));
        app.cache.push(None);
        // Enough transcript below the picture that it can be scrolled past the top of the screen.
        let below: String = (0..60).map(|i| format!("line {i}\n")).collect();
        app.entries.push(Entry::User(below));
        app.cache.push(None);

        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.as_ref().unwrap();
        let (width, height, total) = (seen.area.width, app.view_height, app.total_lines);
        let (_, at) = transcript_with_images(&mut app, width);
        let (marker, _, _) = at[0];

        // Put the marker two lines above the top of the view, so the first two rows of the picture
        // are off screen and the rest of it is visible.
        let want_top = marker + 2;
        app.scroll = total.saturating_sub(height + want_top);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.as_ref().unwrap();
        assert_eq!(
            seen.top, want_top,
            "the marker is off the top of the screen"
        );

        assert_eq!(
            app.images.len(),
            1,
            "the picture is still placed: {:?}",
            app.images
        );
        let placed = &app.images[0];
        assert_eq!(placed.line, marker);
        // And the drawing pass crops it to the rows that are on screen, at the top of the area.
        let (_, draw, after) = crate::tui::plan_images(
            crate::tui::image::Protocol::Kitty,
            &app.drawn,
            &app.images,
            seen.area,
            seen.top,
        );
        assert_eq!(draw.len(), 1, "{draw:?}");
        assert_eq!(draw[0].0, 0, "drawn from the top of the area: {draw:?}");
        assert_eq!(
            after.shown[0].2, 1,
            "one row of the picture is above the fold: {:?}",
            after.shown
        );
    }

    /// The runs of a row as (text, foreground colour), wrapped wide so nothing splits.
    fn block_runs(row: &Row) -> Vec<(String, Option<Color>)> {
        wrap(row, 200)
            .iter()
            .flat_map(|l| {
                l.spans
                    .iter()
                    // The row's own prefix runs are empty; the block's runs are the content.
                    .filter(|s| !s.content.is_empty())
                    .map(|s| (s.content.to_string(), s.style.fg))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// How many distinct foreground colours a row is drawn in.
    fn distinct(runs: &[(String, Option<Color>)]) -> usize {
        let mut seen: Vec<Option<Color>> = runs.iter().map(|(_, c)| *c).collect();
        seen.sort_by_key(|c| format!("{c:?}"));
        seen.dedup();
        seen.len()
    }

    #[test]
    fn the_fence_tag_reaches_the_highlighter() {
        assert_eq!(Language::parse("rust"), Language::Tagged("rust".into()));
        let rows = markdown("```rust\nlet x = 1;\n```", &Theme::default(), 80);
        let runs = block_runs(&rows[0]);
        assert!(
            runs.iter()
                .any(|(t, c)| t == "let" && *c == Some(Theme::default().syntax.keyword)),
            "{runs:?}"
        );
    }

    /// What a prompt is drawn as: its text, and the modifier on each run of it.
    fn prompt(text: &str) -> Vec<(String, Modifier)> {
        let theme = Theme::default();
        let view = View {
            expand: true,
            thinking: false,
            spinner: 0,
            room: (80, 24),
            images: false,
        };
        entry_rows(&Entry::User(text.into()), view, &theme)
            .into_iter()
            .flat_map(|row| row.spans)
            .map(|s| (s.content.into_owned(), s.style.add_modifier))
            .collect()
    }

    /// The runs carrying `modifier`, however else they are styled: `***this***` is both, and asking
    /// for one of the two should still find it.
    fn marked(text: &str, modifier: Modifier) -> Vec<String> {
        prompt(text)
            .into_iter()
            .filter(|(_, m)| m.contains(modifier))
            .map(|(s, _)| s)
            .collect()
    }

    fn drawn(text: &str) -> String {
        prompt(text).into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn a_word_in_asterisks_is_italic_in_a_prompt() {
        // D81: a prompt is read as markdown, which is what pi does with one.
        assert_eq!(marked("make *this* italic", Modifier::ITALIC), ["this"]);
        assert_eq!(marked("**this** is bold", Modifier::BOLD), ["this"]);
        assert_eq!(drawn("make *this* italic"), "make this italic");
        assert_eq!(marked("*one* and *two*", Modifier::ITALIC), ["one", "two"]);
        assert_eq!(marked("*whole line*", Modifier::ITALIC), ["whole line"]);
    }

    #[test]
    fn three_asterisks_are_bold_and_italic_together() {
        // Markdown nests strong around em; the two modifiers patch onto the base rather than one
        // replacing the other, so `***this***` gets both.
        assert_eq!(
            marked("***test***", Modifier::BOLD | Modifier::ITALIC),
            ["test"]
        );
        // All three on one line, each with its own modifier.
        assert_eq!(marked("*a* **b** ***c***", Modifier::ITALIC), ["a", "c"]);
        assert_eq!(marked("*a* **b** ***c***", Modifier::BOLD), ["b", "c"]);
        // The text around it is untouched.
        assert_eq!(drawn("x ***y*** z"), "x y z");
    }

    #[test]
    fn a_prompt_follows_markdown_even_where_that_eats_a_character() {
        // This is CommonMark, and it is what pi's own parser does with the same text: an asterisk
        // inside a word opens emphasis. The test says so rather than hiding it, because it is the
        // one place a prompt is not drawn exactly as it was typed.
        assert_eq!(drawn("2*3*4"), "234");
        assert_eq!(marked("2*3*4", Modifier::ITALIC), ["3"]);
        assert_eq!(
            marked("src/*.rs with tests/*.rs", Modifier::ITALIC),
            [".rs with tests/"]
        );
        // An unmatched asterisk is a literal, as in any markdown.
        assert_eq!(drawn("src/*.rs alone"), "src/*.rs alone");
        assert_eq!(drawn("an *unclosed one"), "an *unclosed one");
    }

    #[test]
    fn a_prompt_is_markdown_in_every_other_way_too() {
        // The marker is on the first row only, and the rest lines up under it.
        let rows = entry_rows(
            &Entry::User("first\nsecond".into()),
            View {
                expand: true,
                thinking: false,
                spinner: 0,
                room: (80, 24),
                images: false,
            },
            &Theme::default(),
        );
        assert_eq!(rows[0].first[0].content.as_ref(), "› ");
        assert_eq!(rows[1].first[0].content.as_ref(), "  ");
        // A fence in a prompt is a code block, as a reply's would be.
        let themed = Theme::default();
        let rows = entry_rows(
            &Entry::User("```rust\nlet x = 1;\n```".into()),
            View {
                expand: true,
                thinking: false,
                spinner: 0,
                room: (80, 24),
                images: false,
            },
            &themed,
        );
        let joined: String = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("let"), "{joined:?}");
        assert!(
            rows.iter()
                .flat_map(|r| r.spans.iter())
                .any(|s| s.style.fg == Some(themed.syntax.keyword)),
            "the fence was not highlighted"
        );
    }

    #[test]
    fn a_prompt_keeps_its_own_colour() {
        // The base style is the prompt's colour, so text that is not emphasised is still the user's,
        // and inline code keeps the code colour rather than taking the prompt's.
        let theme = Theme::default();
        let rows = entry_rows(
            &Entry::User("plain and *marked* and `code`".into()),
            View {
                expand: true,
                thinking: false,
                spinner: 0,
                room: (80, 24),
                images: false,
            },
            &theme,
        );
        let spans: Vec<&Span> = rows.iter().flat_map(|r| r.spans.iter()).collect();
        for span in &spans {
            assert_ne!(span.style.fg, None, "a run lost its colour: {span:?}");
        }
        assert!(
            spans.iter().any(|s| s.content.as_ref().contains("plain")
                && s.style.fg == Some(theme.user_message_text)),
            "{spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content.as_ref() == "code" && s.style.fg == Some(theme.md_code)),
            "{spans:?}"
        );
    }

    #[test]
    fn a_fenced_block_with_a_language_is_highlighted_and_one_without_is_not() {
        // A highlighted line shows more than one colour, and the text is unchanged: highlighting
        // paints, it does not edit (D73).
        let theme = Theme::default();
        let rows = markdown("```rust\nlet x: u32 = foo(1);\n```", &theme, 80);
        let runs = block_runs(&rows[0]);
        let colours = distinct(&runs);
        assert!(colours > 2, "not highlighted: {runs:?}");
        let joined: String = runs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "  let x: u32 = foo(1);");

        // A fence that names no language gets one colour, as it did before highlighting existed.
        let rows = markdown("```\nlet x = 1;\n```", &theme, 80);
        let runs = block_runs(&rows[0]);
        assert_eq!(distinct(&runs), 1, "{runs:?}");
    }

    #[test]
    fn a_multiline_block_comment_stays_a_comment_on_every_line() {
        // The highlighter carries state across lines, so a block's rows are walked in order (D73).
        let theme = Theme::default();
        let rows = markdown("```rust\n/* one\ntwo */ let x = 1;\n```", &theme, 80);
        // The first run of each row is the indent, in the block's own colour; the next is the
        // comment, in the comment role, on both lines.
        for row in rows.iter().take(2) {
            let runs = block_runs(row);
            assert_eq!(runs[0].0, "  ", "{runs:?}");
            assert_eq!(runs[1].1, Some(theme.syntax.comment), "{runs:?}");
        }
        // After `*/` the same line is code again.
        let runs = block_runs(&rows[1]);
        assert!(
            runs.iter()
                .any(|(t, c)| t == "let" && *c == Some(theme.syntax.keyword)),
            "{runs:?}"
        );
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrapping_keeps_indent_and_splits_long_words() {
        let row = Row::prefixed("› ", "  ", Style::new(), vec![Span::raw("aaa bbb ccc")]);
        assert_eq!(text(&wrap(&row, 9)), ["› aaa bbb", "  ccc"]);
        let code = Row::plain(vec![Span::raw("    fn x()")]);
        assert_eq!(text(&wrap(&code, 40)), ["    fn x()"]);
        let long = Row::plain(vec![Span::raw("abcdefghij")]);
        assert_eq!(text(&wrap(&long, 4)), ["abcd", "efgh", "ij"]);
        let styled = Row::plain(vec![
            Span::raw("one "),
            Span::styled("two", code_style(&Theme::default())),
        ]);
        let lines = wrap(&styled, 20);
        assert_eq!(lines[0].spans.len(), 2);
    }

    #[test]
    fn markdown_blocks() {
        let md = "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n  wrapped\n\n```rust\nfn main() {\n    x();\n}\n```\n\n| a | bb |\n|---|----|\n| ccc | d |\n";
        let rows: Vec<String> = markdown(md, &Theme::default(), 80)
            .iter()
            .flat_map(|r| text(&wrap(r, 80)))
            .collect();
        assert_eq!(
            rows,
            [
                "Title",
                "",
                "Some bold and code.",
                "",
                "• one",
                "• two",
                "  wrapped",
                "",
                "  fn main() {",
                "      x();",
                "  }",
                "",
                "a   │ bb",
                "────┼───",
                "ccc │ d ",
            ]
        );
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(keep_tail("/a/b/project", 20), "/a/b/project");
        assert_eq!(keep_tail("/very/long/path/project", 10), "…h/project");
        assert_eq!(keep_tail("/x", 0), "");
    }

    /// Every row of a rendered table, as text, so a test can see the alignment.
    fn table_text(text: &str, room: usize) -> Vec<String> {
        markdown(text, &Theme::default(), room)
            .iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_table_is_fitted_to_the_room_instead_of_wrapped_apart() {
        // The failure this fixes: a table wider than the terminal used to be handed to the generic
        // wrapper, which broke its rows into pieces and destroyed the columns.
        let md = "| a | b | c |\n|---|---|---|\n| alpha | beta | gamma |";
        for room in [80usize, 40, 24, 16] {
            let rows = table_text(md, room);
            for row in &rows {
                assert!(
                    row.width() <= room.max(4),
                    "room {room}: {row:?} is {} wide",
                    row.width()
                );
            }
            // The rule and the separators survive, which is what says it is still a table.
            assert!(
                rows.iter().any(|r| r.contains('┼')),
                "room {room}: {rows:?}"
            );
            assert!(
                rows.iter().any(|r| r.contains('│')),
                "room {room}: {rows:?}"
            );
        }
    }

    #[test]
    fn a_narrow_column_wraps_its_cell_rather_than_pushing_the_row_off() {
        // Long prose in one column of a narrow table: the cell becomes several lines and the other
        // columns still line up beside it.
        let md = "| gap | pi | opencode | saddle |\n|---|---|---|---|\n\
                  | plugin system | TypeScript extensions with registerTool and lifecycle events | plugins and custom tools | none yet |";
        let rows = table_text(md, 60);
        assert!(rows.iter().all(|r| r.width() <= 60), "{rows:#?}");
        // The first column appears on every physical line of that row, so it lines up.
        let saddles = rows.iter().filter(|r| r.contains("saddle")).count();
        assert!(saddles >= 1, "{rows:#?}");
        // The word pair wraps at its space rather than being split, which is the difference between
        // a narrow table and a mangled one.
        assert!(rows.iter().any(|r| r.contains("plugin")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("system")), "{rows:#?}");
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("plugi ") && !r.contains("plugin")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_table_that_fits_keeps_its_natural_widths() {
        // Fitting only shrinks: on a wide terminal every cell is padded to its own content, so a
        // short table does not get spread across the screen.
        let widths = column_widths(&[3, 6, 4], &[3, 6, 4], 80);
        assert_eq!(widths, vec![3, 6, 4]);
        // A wide one shrinks, and never below the longest word while those fit at all.
        let wide = column_widths(&[60, 60, 60], &[8, 8, 8], 40);
        assert!(wide.iter().sum::<usize>() <= 40, "{wide:?}");
        assert!(wide.iter().all(|w| *w >= 8), "{wide:?}");
        // A column whose longest word fits inside its fair share keeps the word whole, while the
        // prose column beside it gives up the room: `yes` must not become `ye`/`s`.
        let mixed = column_widths(&[4, 40, 5], &[4, 8, 5], 30);
        assert_eq!(mixed[0], 4, "{mixed:?}");
        assert_eq!(mixed[2], 5, "{mixed:?}");
        assert!(mixed[1] < 40, "{mixed:?}");
        // When even the longest words do not fit, the columns still have a shape and each has a
        // cell, rather than the table running off the screen.
        let tiny = column_widths(&[20, 20], &[20, 20], 3);
        assert_eq!(tiny.len(), 2);
        assert!(tiny.iter().all(|w| *w >= 1), "{tiny:?}");
        assert!(tiny.iter().sum::<usize>() <= 3, "{tiny:?}");
        // An empty table is not a panic.
        assert_eq!(column_widths(&[], &[], 40), Vec::<usize>::new());
    }

    #[test]
    fn tool_blocks_show_the_call_and_the_useful_end_of_the_output() {
        let block = |name: &str, detail: &str, ok: bool, content: &str, expand: bool| {
            let (rows, bg) = tool_block(
                name,
                detail,
                Some(&(ok, content.into())),
                None,
                expand,
                0,
                &Theme::default(),
            );
            let lines: Vec<String> = rows.iter().flat_map(|r| text(&wrap(r, 80))).collect();
            (lines, bg)
        };
        let output = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let passing = format!("exit code 0\n--- stdout ---\n{output}");
        let (lines, bg) = block("bash", "cargo test", true, &passing, false);
        assert_eq!(bg, Theme::default().tool_success_bg);
        assert_eq!(
            lines,
            [
                "$ cargo test",
                "",
                "… 5 earlier lines · ctrl+o shows all",
                "line 6",
                "line 7",
                "line 8",
                "line 9",
                "line 10",
            ]
        );
        let (lines, _) = block("bash", "cargo test", true, &passing, true);
        assert_eq!(lines.len(), 12, "expanded: header, gap, ten lines");

        let failing = "exit code 101\n--- stderr ---\nerror: boom";
        let (lines, bg) = block("bash", "cargo build", false, failing, false);
        assert_eq!(bg, Theme::default().tool_error_bg);
        assert_eq!(lines, ["$ cargo build", "", "error: boom", "exit code 101"]);

        let (lines, _) = block("read", "src/x.rs", true, "a\nb\nc", false);
        assert_eq!(lines, ["read src/x.rs", "", "3 lines"]);
        let (lines, _) = block("write", "a.txt", true, "wrote 2 bytes to a.txt", false);
        assert_eq!(lines, ["write a.txt", "", "wrote 2 bytes to a.txt"]);

        let (rows, bg) = tool_block("bash", "sleep 9", None, None, false, 0, &Theme::default());
        let diff = "~\n-12 old line\n+12 new line, long enough to wrap around\n 13 next\n";
        let (edit_rows, _) = tool_block(
            "edit",
            "src/x.rs",
            Some(&(true, "edited src/x.rs at line 12".into())),
            Some(diff),
            false,
            0,
            &Theme::default(),
        );
        let shown: Vec<String> = edit_rows.iter().flat_map(|r| text(&wrap(r, 30))).collect();
        assert_eq!(
            shown,
            [
                "edit src/x.rs",
                "",
                "     ⋯",
                "-12 │ old line",
                "+12 │ new line, long enough to",
                "    │ wrap around",
                " 13 │ next",
            ]
        );
        assert_eq!(bg, Theme::default().tool_pending_bg);
        assert_eq!(text(&wrap(&rows[0], 80)), ["⠋ $ sleep 9"]);
    }

    #[test]
    fn shading_spans_the_full_width_with_padding() {
        let rows = vec![Row::plain(vec![Span::raw("$ ls")])];
        let lines = shaded(&rows, Theme::default().tool_success_bg, 20);
        assert_eq!(
            text(&lines),
            [
                " ".repeat(20),
                format!(" $ ls{}", " ".repeat(15)),
                " ".repeat(20)
            ]
        );
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .all(|s| s.style.bg == Some(Theme::default().tool_success_bg))
        );
    }

    #[test]
    fn a_catppuccin_flavour_paints_the_screen() {
        // The palette has to reach the renderer, not just build (D76): the frame in Mocha's mauve,
        // its own text colour, and syntax in its own colours.
        let tables = crate::tui::theme::ThemeTables::new();
        let mut app = bare_app();
        app.theme = crate::tui::theme::build("catppuccin-mocha", &tables).unwrap();
        app.entries.push(Entry::User("hello".into()));
        app.cache.push(None);
        app.entries
            .push(Entry::Assistant("```rust\nlet x = 1;\n```".into()));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let seen: Vec<Color> = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].fg)
            .collect();
        let mauve = Color::Rgb(0xcb, 0xa6, 0xf7);
        let keyword = Color::Rgb(0xcb, 0xa6, 0xf7);
        let number = Color::Rgb(0xfa, 0xb3, 0x87);
        assert!(seen.contains(&mauve), "the frame is Mocha's mauve");
        assert!(seen.contains(&number), "the number is Mocha's peach");
        assert!(seen.contains(&keyword), "the keyword is Mocha's mauve");
        // And nothing from the old default palette's own syntax colours is left.
        assert!(
            !seen.contains(&Color::Rgb(165, 214, 255)),
            "the previous palette's string colour is gone"
        );
    }

    #[test]
    fn a_sign_in_on_screen_shows_the_address_and_the_code() {
        // D85: the code is the whole point of the screen, so it has to be legible there rather than
        // only in a log. The dialog is drawn with the code the host reported, and with the error
        // where the code was when the sign-in fails.
        let mut app = bare_app();
        let choice = |name: &str, oauth: bool| crate::tui::login::Choice {
            name: name.into(),
            detail: String::new(),
            status: if oauth {
                "not signed in".into()
            } else {
                String::new()
            },
            needs_key: !oauth,
            has_key: false,
            model: None,
            custom: false,
            oauth,
        };
        let mut login = crate::tui::login::Login::new(
            vec![choice("openai", false), choice("openai-chatgpt", true)],
            vec![],
            Some("openai-chatgpt"),
            true,
            "~/.local/state/saddle/auth.json".into(),
        );
        let Some(crate::tui::login::Outcome::SignIn(id, _)) = login.take_pending() else {
            panic!("a sign-in to start");
        };
        app.open_login(login);
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Log in to openai-chatgpt"), "{text}");
        assert!(text.contains("asking for a sign-in code"), "{text}");

        app.login.as_mut().unwrap().device_code(
            id,
            Ok((
                "https://auth.openai.com/codex/device".into(),
                "FPAT-OKEE6".into(),
            )),
        );
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("https://auth.openai.com/codex/device"),
            "{text}"
        );
        assert!(
            text.contains("FPAT-OKEE6"),
            "the code is on screen:\n{text}"
        );
        assert!(text.contains("waiting for the sign-in"), "{text}");
        assert!(text.contains("o open · c copy link · esc cancel"), "{text}");
        assert!(
            text.contains("ready to paste"),
            "the clipboard line must fit the box rather than be cut:\n{text}"
        );

        app.login
            .as_mut()
            .unwrap()
            .signed_in(id, Err("the login code expired".into()));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("the login code expired"), "{text}");
        assert!(
            !text.contains("FPAT-OKEE6"),
            "the old code is gone:\n{text}"
        );
    }

    #[test]
    fn a_drag_inside_a_dialog_copies_the_dialog_text() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        // D87: a dialog owns the screen while it is open, so a drag there means its text. Without
        // this the text was drawn and unselectable, which is what the sign-in screen made obvious.
        let mut app = bare_app();
        app.settings = Some(crate::tui::settings::SettingsDialog::new(
            crate::tui::settings::Values {
                provider: "synthetic".into(),
                model: "hf:zai-org/GLM-5.3".into(),
                effort: None,
                max_tokens: None,
                context_window: None,
                window: 128_000,
                status: String::new(),
                max_steps: None,
                auto_compact: true,
                idle_compact: true,
                background_compact: true,
                show_reasoning: true,
                expand_tools: false,
                config_path: "~/.config/saddle/config.toml".into(),
                policy_path: "~/.config/saddle/config.toml".into(),
            },
        ));
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.dialog_seen.clone().expect("the dialog's text was kept");
        let wanted = seen
            .lines
            .iter()
            .find(|l| l.contains("provider"))
            .cloned()
            .unwrap_or_else(|| panic!("no provider row in {:?}", seen.lines));
        // Drag from the start of that row to its end, the way a mouse would.
        let row = seen
            .lines
            .iter()
            .position(|l| l.contains("provider"))
            .unwrap() as u16
            + seen.area.y;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: seen.area.x,
            row,
            modifiers: KeyModifiers::NONE,
        });
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: seen.area.right().saturating_sub(1),
            row,
            modifiers: KeyModifiers::NONE,
        });
        let copied = app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: seen.area.right().saturating_sub(1),
            row,
            modifiers: KeyModifiers::NONE,
        });
        let Some(crate::tui::app::Action::Copy(text, what)) = copied else {
            panic!("a drag in the dialog must copy, got {copied:?}");
        };
        assert_eq!(what, "the selection");
        assert_eq!(text, wanted.trim(), "the whole row, as drawn");

        // The selection is drawn where it is: the highlight is on the buffer, not just in state.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let at = (seen.area.x, row);
        assert_eq!(
            buf[at].bg, app.theme.selected_bg,
            "the first cell of the row is highlighted"
        );
    }

    #[test]
    fn a_drag_in_a_dialog_highlights_nothing_in_the_transcript() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        // The transcript is shaded before the dialogs are drawn, and a dialog's selection records
        // line numbers in the dialog's own frame. Shading the transcript with them painted a second
        // highlight near the top of the screen, over unrelated text (D87).
        let mut app = bare_app();
        // A short transcript, so the lines the dialog's indices name are on screen: with a long one
        // they are scrolled away and the bug would not show, which is why it surfaced on a fresh
        // session (D87).
        for i in 0..3 {
            app.entries
                .push(Entry::User(format!("transcript line {i}")));
            app.cache.push(None);
        }
        app.settings = Some(crate::tui::settings::SettingsDialog::new(
            crate::tui::settings::Values {
                provider: "synthetic".into(),
                model: "m".into(),
                effort: None,
                max_tokens: None,
                context_window: None,
                window: 1,
                status: String::new(),
                max_steps: None,
                auto_compact: true,
                idle_compact: true,
                background_compact: true,
                show_reasoning: true,
                expand_tools: false,
                config_path: "c".into(),
                policy_path: "c".into(),
            },
        ));
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.dialog_seen.clone().unwrap();
        // Select the dialog's first two rows.
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: seen.area.x,
            row: seen.area.y,
            modifiers: KeyModifiers::NONE,
        });
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: seen.area.x + 6,
            row: seen.area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let highlight = app.theme.selected_bg;
        // Everything outside the dialog's box is unhighlighted: the transcript saw none of it.
        let mut outside = 0;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let inside = seen.area.contains(ratatui::layout::Position::new(x, y));
                if !inside && buf[(x, y)].bg == highlight {
                    outside += 1;
                }
            }
        }
        assert_eq!(
            outside, 0,
            "the highlight must not appear outside the dialog"
        );
        // And inside it, the two rows are.
        assert_eq!(buf[(seen.area.x, seen.area.y)].bg, highlight);
        assert_eq!(buf[(seen.area.x, seen.area.y + 1)].bg, highlight);
    }

    #[test]
    fn a_drag_past_a_dialog_stops_at_its_edge_and_closing_drops_it() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = bare_app();
        app.entries.push(Entry::User("a transcript line".into()));
        app.cache.push(None);
        app.settings = Some(crate::tui::settings::SettingsDialog::new(
            crate::tui::settings::Values {
                provider: "synthetic".into(),
                model: "m".into(),
                effort: None,
                max_tokens: None,
                context_window: None,
                window: 1,
                status: String::new(),
                max_steps: None,
                auto_compact: true,
                idle_compact: true,
                background_compact: true,
                show_reasoning: true,
                expand_tools: false,
                config_path: "c".into(),
                policy_path: "c".into(),
            },
        ));
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.dialog_seen.clone().unwrap();
        let first = seen.area.y;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: seen.area.x,
            row: first,
            modifiers: KeyModifiers::NONE,
        });
        // Dragged far below the box: the selection stops at the last line it drew rather than
        // reaching into the transcript underneath, which is not what the mouse was over.
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: seen.area.x + 4,
            row: 29,
            modifiers: KeyModifiers::NONE,
        });
        let sel = app.selection.expect("a selection");
        assert_eq!(sel.focus.line, seen.lines.len() - 1);
        assert!(
            !app.selected_text()
                .unwrap_or_default()
                .contains("a transcript line"),
            "the transcript must not be in the copy"
        );

        // A dialog that closes leaves its selection behind: the same indices would land on other
        // text.
        app.settings = None;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.dialog_seen, None);
        assert_eq!(app.selection, None);

        // And so does a dialog that opens over one the transcript had: the same indices would land
        // on the dialog's text instead.
        app.entries
            .push(Entry::User("another transcript line".into()));
        app.cache.push(None);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let transcript = app.seen.clone().unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: transcript.area.x,
            row: transcript.area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.selection.is_some(),
            "the transcript is selectable again"
        );
        app.settings = Some(crate::tui::settings::SettingsDialog::new(
            crate::tui::settings::Values {
                provider: "synthetic".into(),
                model: "m".into(),
                effort: None,
                max_tokens: None,
                context_window: None,
                window: 1,
                status: String::new(),
                max_steps: None,
                auto_compact: true,
                idle_compact: true,
                background_compact: true,
                show_reasoning: true,
                expand_tools: false,
                config_path: "c".into(),
                policy_path: "c".into(),
            },
        ));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.selection, None, "the old selection did not survive");
    }

    #[test]
    fn the_model_picker_shows_a_filtered_list_and_names_the_model_in_force() {
        // D88: the point of the picker is that the name does not have to be known, so the screen has
        // to show the served list, the filter being typed, and which model is current.
        let mut app = bare_app();
        let mut choice = crate::tui::login::Choice {
            name: "synthetic".into(),
            detail: String::new(),
            status: "logged in".into(),
            needs_key: true,
            has_key: true,
            model: Some("gpt-5.6-sol".into()),
            custom: false,
            oauth: false,
        };
        choice.model = Some("gpt-5.6-sol".into());
        let mut login =
            crate::tui::login::Login::for_model(vec![choice], "synthetic", "auth.json".into());
        let Some(crate::tui::login::Outcome::Check(id, _)) = login.take_pending() else {
            panic!("a listing to ask for");
        };
        login.models(
            id,
            Ok(vec![
                "gpt-5.6-luna".into(),
                "gpt-5.6-sol".into(),
                "gpt-6.1-terra".into(),
            ]),
        );
        app.open_login(login);
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Model for synthetic"), "{text}");
        assert!(
            text.contains("gpt-5.6-sol"),
            "the model in force is named:\n{text}"
        );
        for m in ["gpt-5.6-luna", "gpt-6.1-terra"] {
            assert!(text.contains(m), "{m} is missing:\n{text}");
        }
        assert!(text.contains("type to filter"), "{text}");
        assert!(
            text.contains("esc close"),
            "a picker has nothing behind it:\n{text}"
        );
        // The current model is where the highlight starts, so enter keeps it by accident only if
        // nothing was chosen.
        assert_eq!(
            app.login.as_ref().unwrap().stage,
            crate::tui::login::Stage::Models {
                models: vec![
                    "gpt-5.6-luna".into(),
                    "gpt-5.6-sol".into(),
                    "gpt-6.1-terra".into()
                ],
                filter: String::new(),
                selected: 1,
                note: None,
            }
        );
        // Typing narrows what is drawn.
        for c in "terra".chars() {
            app.key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("filter  › terra"), "{text}");
        assert!(
            !text.contains("gpt-5.6-luna"),
            "the filter hides the rest:\n{text}"
        );
        assert!(text.contains("gpt-6.1-terra"), "{text}");
    }

    #[test]
    fn the_sign_in_link_is_a_thing_a_click_can_open() {
        // D86: a drawn link is the terminal's to ignore, and a dialog's text is not selectable, so
        // the address has to be a target. What is checked here is that the cells the click lands on
        // are the cells the address was drawn in.
        let mut app = bare_app();
        let choice = crate::tui::login::Choice {
            name: "openai-chatgpt".into(),
            detail: String::new(),
            status: "not signed in".into(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        };
        let mut login = crate::tui::login::Login::new(
            vec![choice],
            vec![],
            Some("openai-chatgpt"),
            true,
            "auth.json".into(),
        );
        let Some(crate::tui::login::Outcome::SignIn(id, _)) = login.take_pending() else {
            panic!("a sign-in to start");
        };
        app.open_login(login);
        // Nothing to click until the server names an address.
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.open_url_at, None);

        app.login.as_mut().unwrap().device_code(
            id,
            Ok((
                "https://auth.openai.com/codex/device".into(),
                "FPAT-OKEE6".into(),
            )),
        );
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let at = app
            .open_url_at
            .expect("the link was drawn, so it has cells");
        let screen = terminal.backend().buffer();
        // The first cell of the target is the first character of the address.
        assert_eq!(
            screen[(at.x, at.y)].symbol(),
            "h",
            "the target starts at the link"
        );
        let start = at.x + at.width - 1;
        assert_eq!(screen[(start, at.y)].symbol(), "e", "and ends with it");
        // Clicking it asks to open that address; clicking the code does not.
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let up = |column, row| MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // A click is a press and a release in the same place: that is what follows the link.
        assert_eq!(
            app.mouse(click(at.x + 3, at.y)),
            None,
            "pressing selects nothing yet"
        );
        assert_eq!(
            app.mouse(up(at.x + 3, at.y)),
            Some(crate::tui::app::Action::OpenUrl(
                "https://auth.openai.com/codex/device".into()
            ))
        );
        assert_eq!(app.mouse(click(at.x, at.y + 1)), None, "that is the code");
        app.mouse(up(at.x, at.y + 1));
        assert_eq!(app.mouse(click(0, 0)), None);
        app.mouse(up(0, 0));

        // A drag that *starts* on the link selects it, which is what it could not do while the
        // press was swallowed by the link (D87).
        app.mouse(click(at.x, at.y));
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: at.x + 4,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        });
        let copied = app.mouse(up(at.x + 4, at.y));
        let Some(crate::tui::app::Action::Copy(text, _)) = copied else {
            panic!("a drag on the link must select it, got {copied:?}");
        };
        assert!(text.starts_with("http"), "{text}");
    }

    #[test]
    fn the_theme_list_is_drawn_with_the_theme_it_offers() {
        // D74: the point of the picker is that you see the choice before making it, so the box has
        // to be drawn in the highlighted theme and say which one that is.
        let mut app = bare_app();
        let tables: crate::tui::theme::ThemeTables =
            toml::from_str("[mine]\nborder = \"#ff0000\"\n").unwrap();
        let name = app.theme.name.clone();
        app.open_themes(crate::tui::themes::ThemePicker::collect(&tables, &name));
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Theme"), "{text}");
        for entry in ["default", "dark", "light", "mono", "system", "mine"] {
            assert!(text.contains(entry), "{entry} is not in the list:\n{text}");
        }
        assert!(text.contains("›"), "the highlight is marked:\n{text}");
        assert!(text.contains("built in"), "the source is shown:\n{text}");
        assert!(text.contains("config.toml"), "{text}");

        // Moving to the config theme draws the box with its border colour, which is the preview.
        while app.themes.as_ref().unwrap().current().name != "mine" {
            app.key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Down,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(
            app.theme.border,
            Color::Rgb(0xff, 0x00, 0x00),
            "the drawing is now `mine`"
        );
        // And the box itself carries that colour, so the list is read in what it offers.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let reds = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].fg == Color::Rgb(0xff, 0x00, 0x00))
            .count();
        assert!(reds > 0, "the frame is drawn in the previewed theme");
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn bare_app() -> App {
        App::new(Header {
            provider: "mock".into(),
            model: "mock-model".into(),
            workspace: "/ws".into(),
            audit: "/a.jsonl".into(),
            notes: Vec::new(),
            show_reasoning: true,
            expand_tools: false,
            max_steps: None,
            context_window: 128_000,
            priced: false,
            mouse: false,
            status: crate::tui::status::Template::default(),
            images: None,
            theme: None,
        })
    }

    #[test]
    fn reasoning_shows_dimmed_and_folds_away() {
        let mut app = bare_app();
        app.agent(AgentEvent::Reasoning(
            "\nFirst, read the file.\nThen fix it.".into(),
        ));
        app.agent(AgentEvent::Text("Fixed.".into()));
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(
            all.contains("│ First, read the file.\n│ Then fix it.\n\nFixed."),
            "{all}"
        );
        app.show_thinking = false;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(
            all.contains("∴ thought for 2 lines · ctrl+t shows it"),
            "{all}"
        );
        assert!(!all.contains("First, read"), "{all}");
    }

    #[test]
    fn the_selection_is_drawn_highlighted() {
        let mut app = bare_app();
        app.agent(AgentEvent::Text(
            "a selected reply\nsplit over two lines".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // The first reply line, wherever the last draw put it.
        let area = app.seen.as_ref().expect("transcript geometry").area;
        let first = app
            .seen
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .position(|l| l.starts_with("a selected"))
            .expect("the reply line");

        app.selection = Some(super::super::app::Selection {
            anchor: super::super::app::Cell { line: first, ch: 2 },
            focus: super::super::app::Cell {
                line: first,
                ch: 10,
            },
            dragging: false,
        });
        let top = app.seen.as_ref().unwrap().top;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = area.y + (first - top) as u16;
        assert_eq!(buf[(0, row)].bg, Color::Reset, "before the anchor");
        assert_eq!(buf[(2, row)].bg, Color::Rgb(42, 74, 112));
        assert_eq!(buf[(9, row)].bg, Color::Rgb(42, 74, 112));
        assert_eq!(buf[(10, row)].bg, Color::Reset, "past the focus");
    }

    #[test]
    fn shade_splits_spans_where_the_selection_starts_and_ends() {
        let mut line = Line::from(vec![
            Span::raw("abc"),
            Span::styled("def", Style::new().fg(Theme::default().error)),
        ]);
        shade(&mut line, 2, 5, &Theme::default());
        assert_eq!(plain(&line), "abcdef", "the text is unchanged");
        let highlighted: String = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Rgb(42, 74, 112)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(highlighted, "cde", "only the covered characters");
        let plain_part: String = line
            .spans
            .iter()
            .filter(|s| s.style.bg.is_none())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(plain_part, "abf");
        // The text's own colour survives outside the selection.
        assert_eq!(line.spans.last().unwrap().style.fg, Some(Color::Red));
        // A whole-line selection needs no splitting.
        let mut whole = Line::from(vec![Span::raw("xy")]);
        shade(&mut whole, 0, usize::MAX, &Theme::default());
        assert_eq!(whole.spans.len(), 1);
        assert_eq!(whole.spans[0].style.bg, Some(Color::Rgb(42, 74, 112)));
    }

    #[test]
    fn the_permissions_editor_lists_what_the_project_grants() {
        use crate::tui::permissions::{Permissions, Row};
        let mut app = bare_app();
        app.permissions = Some(Permissions::new(
            vec![
                Row {
                    rule: "agent.fs-read[0]".into(),
                    kind: crate::tui::permissions::Kind::FsRead,
                    root: "workspace".into(),
                    exclude: ".env*".into(),
                    on: true,
                    editable: true,
                    locked: None,
                },
                Row {
                    rule: "agent.exec".into(),
                    kind: crate::tui::permissions::Kind::Exec,
                    root: String::new(),
                    exclude: String::new(),
                    on: true,
                    editable: false,
                    locked: Some("exec is granted in the policy file".into()),
                },
            ],
            "~/.config/saddle/policies/home%me%proj.toml".into(),
            "~/.config/saddle/policy.toml".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        for want in [
            "Permissions for this project",
            "› agent.fs-read[0]",
            "read workspace  exclude [.env*]",
            // A row that cannot be changed is still listed, and marked.
            "agent.exec      ·",
            "run commands",
            "home%me%proj.toml",
        ] {
            assert!(all.contains(want), "missing {want:?} in\n{all}");
        }
        // A row that cannot be changed says why when it is the one selected.
        if let Some(dialog) = &mut app.permissions {
            dialog.selected = 1;
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(all.contains("exec is granted in the policy file"), "{all}");
    }

    #[test]
    fn a_theme_paints_the_screen_so_a_custom_one_actually_shows() {
        // D71: the theme is not a decoration on the settings struct — every role the renderer uses
        // comes from it, so a theme that sets `accent` and `muted` changes what is drawn.
        let mut app = bare_app();
        app.entries = vec![Entry::User("hello".into())];
        app.theme = Theme {
            accent: Color::Rgb(255, 0, 255),
            muted: Color::Rgb(0, 255, 0),
            ..Theme::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let painted = |colour: Color| {
            (0..buffer.area.height)
                .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].fg == colour))
        };
        assert!(
            painted(Color::Rgb(255, 0, 255)),
            "the accent is used somewhere"
        );
        assert!(painted(Color::Rgb(0, 255, 0)), "and the muted colour");

        // The default theme still paints cyan, which is what most of this renderer's accent is.
        let mut app = bare_app();
        app.entries = vec![Entry::User("hello".into())];
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let cyan = (0..buffer.area.height)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].fg == Color::Cyan));
        assert!(cyan, "the default theme is unchanged");
    }

    #[test]
    fn the_built_in_themes_build_and_the_light_one_differs() {
        // The shipped themes must be usable, not just parseable: a theme with a role left out would
        // be a colour nobody chose.
        for name in crate::tui::theme::builtin_names() {
            let theme = crate::tui::theme::build(name, &crate::tui::theme::ThemeTables::new())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(theme.name, name);
        }
        let light =
            crate::tui::theme::build("light", &crate::tui::theme::ThemeTables::new()).unwrap();
        let default = Theme::default();
        assert_ne!(light.accent, default.accent, "light is a different theme");
        assert_ne!(
            light.tool_pending_bg, default.tool_pending_bg,
            "and not just the accent"
        );
    }

    #[test]
    fn the_slash_menu_sits_above_the_editor() {
        let mut app = bare_app();
        app.editor.set("/lo".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rows: Vec<String> = screen(&terminal).lines().map(str::to_owned).collect();
        // Status line at 19, editor at 16..=18, so the menu's last row is 15. Four matches now
        // (/login, /logout, /clone, /reload), so the list starts one row higher.
        assert!(rows[15].starts_with("╰ ↑↓ select"), "{}", rows[15]);
        assert!(
            rows[11].contains("› /login") && rows[11].contains("choose a provider"),
            "{}",
            rows[11]
        );
        assert!(rows[12].contains("  /logout [provider]"), "{}", rows[12]);
        assert!(rows[13].contains("  /clone"), "{}", rows[13]);
        assert!(rows[14].contains("  /reload"), "{}", rows[14]);
    }

    #[test]
    fn the_tree_draws_every_message_with_here_marked_and_other_branches_dimmed() {
        use crate::store::{Point, PointKind};
        use crate::tui::tree::Tree;
        let point = |node, kind, preview: &str, here, active| Point {
            node,
            parent: None,
            kind,
            preview: preview.into(),
            text: String::new(),
            here,
            active,
        };
        let mut app = bare_app();
        app.tree = Some(Tree::new(vec![
            point(1, PointKind::User, "fix the build", false, true),
            point(2, PointKind::Assistant, "looking", false, true),
            point(3, PointKind::Tool, "tool results", false, true),
            point(4, PointKind::User, "a branch left behind", false, false),
            point(5, PointKind::Summary, "we compacted here", true, true),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(all.contains("Conversation tree"), "{all}");
        assert!(all.contains("❯ fix the build"), "{all}");
        assert!(all.contains("◆ looking"), "{all}");
        assert!(all.contains("⚙ tool results"), "{all}");
        // The message the conversation is at, and one on a branch it would not carry (D54).
        assert!(all.contains("⋯ we compacted here   ← here"), "{all}");
        assert!(
            all.contains("❯ a branch left behind   (other branch)"),
            "{all}"
        );
        assert!(all.contains("enter continue here"), "{all}");
    }

    #[test]
    fn settings_and_resume_dialogs_draw() {
        use crate::store::Summary;
        use crate::tui::settings::Values;
        let mut app = bare_app();
        app.settings = Some(SettingsDialog::new(Values {
            provider: "synthetic".into(),
            model: "hf:zai-org/GLM-5.3".into(),
            effort: Some("high".into()),
            max_tokens: None,
            context_window: None,
            window: 262_144,
            status: crate::tui::status::DEFAULT_STATUS.into(),
            max_steps: None,
            auto_compact: true,
            idle_compact: true,
            background_compact: true,
            show_reasoning: true,
            expand_tools: false,
            config_path: "~/.config/saddle/config.toml".into(),
            policy_path: "~/.config/saddle/policy.toml".into(),
        }));
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        for want in [
            "Settings",
            "› Provider",
            "  Reasoning effort",
            "  Max output tokens",
            "  Show reasoning",
            "  Permissions for this project",
            "enter opens /login",
        ] {
            assert!(all.contains(want), "missing {want:?} in\n{all}");
        }

        app.settings = None;
        let now = std::time::SystemTime::now();
        app.resume = Some(SessionPicker::new(vec![Summary {
            id: "a".into(),
            title: "fix the flaky test in the sandbox crate".into(),
            name: None,
            prompts: 3,
            messages: 9,
            model: "mock-model".into(),
            forked_from: None,
            updated: now - std::time::Duration::from_secs(600),
        }]));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(all.contains("Resume a session"), "{all}");
        assert!(
            all.contains("› fix the flaky test in the sandbox crate"),
            "{all}"
        );
        assert!(all.contains("10m ago · 3 prompts"), "{all}");
    }

    #[test]
    fn the_scroll_badge_offers_a_jump_and_records_where_it_is() {
        let mut app = bare_app();
        for i in 0..40 {
            app.note(NoteKind::Info, &format!("line {i}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.jump_badge, None, "at the bottom there is no badge");
        app.scroll = 10;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(
            all.contains("↓ 10 more lines · end or click jumps to the end"),
            "{all}"
        );
        let badge = app.jump_badge.expect("badge drawn");
        assert_eq!(
            badge.y, 14,
            "on the last transcript row, above the activity line"
        );
        assert_eq!(badge.right(), 80);
    }

    #[test]
    fn draws_a_session() {
        let mut app = App::new(Header {
            provider: "mock".into(),
            model: "mock-model".into(),
            workspace: "/ws".into(),
            audit: "/state/audit/s.jsonl".into(),
            notes: Vec::new(),
            show_reasoning: true,
            expand_tools: false,
            max_steps: None,
            context_window: 128_000,
            priced: false,
            mouse: false,
            status: crate::tui::status::Template::default(),
            images: None,
            theme: None,
        });
        app.entries.push(Entry::User("fix the build".into()));
        app.cache.push(None);
        app.busy = Some(Activity::Waiting);
        app.agent(AgentEvent::Text("Looking at **it**.".into()));
        app.agent(AgentEvent::Call {
            name: "bash".into(),
            detail: "cargo build".into(),
        });
        app.agent(AgentEvent::Result {
            images: Vec::new(),
            ok: false,
            content: "denied: agent has no exec grant".into(),
            diff: None,
        });
        app.agent(AgentEvent::Reply {
            usage: crate::provider::Usage {
                input: 1200,
                output: 80,
                cache_read: 0,
                cache_write: 0,
            },
            cost: 0.0,
        });
        app.agent(AgentEvent::TurnEnd(End::Answered));
        app.editor.set("next".into());

        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen: Vec<String> = {
            let buf = terminal.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_owned())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect()
        };
        let all = screen.join("\n");
        for want in [
            "saddle · mock · mock-model",
            "› fix the build",
            "Looking at it.",
            " $ cargo build",
            " denied: agent has no exec grant",
            "│next",
            "↑1.2k ↓80 cache 0 · ctx 1.3k/128.0k · mock mock-model",
        ] {
            assert!(all.contains(want), "missing {want:?} in\n{all}");
        }
        assert!(screen[19].starts_with("/ws"), "{}", screen[19]);
        // The refused call sits on the error shade, edge to edge.
        let row = screen
            .iter()
            .position(|l| l.contains("denied: agent"))
            .unwrap() as u16;
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, row)].bg, Theme::default().tool_error_bg);
        assert_eq!(buf[(59, row)].bg, Theme::default().tool_error_bg);
        assert_eq!(terminal.backend().cursor_position(), Position::new(5, 17));
    }
}
