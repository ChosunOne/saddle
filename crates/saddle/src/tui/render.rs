//! Drawing: the transcript, the queue, the activity line, the editor and the status line.

use crate::plugins::services::panel::{PanelView, Row as PanelRow, Tone};
use crate::provider;
use crate::provider::Image;
use crate::tasks::elapsed_text as tasks_elapsed_text;
use crate::tasks::{State as TaskState, View as TaskView};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use std::borrow::Cow;
use std::env;
use std::iter;
use std::mem;
use std::path::Path;
use std::time::{Instant, SystemTime};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    Activity, App, COPY_NOTE, Cached, CompactState, CompactionView, Confirm, Entry, NoteKind,
    Placed, Seen, Source,
};
use super::ask::{AskDialog, Stage as AskStage};
use super::login::{Login, MODEL_ROWS, Purpose, Stage, matching};
use super::permissions::{ADD_HELP, Draft, Kind, Permissions};
use super::sessions::{ROWS as SESSION_ROWS, SessionPicker, age};
use super::settings::{ROWS, SettingsDialog};
use super::themes::{ROWS as THEME_ROWS, ThemePicker};
use super::tree::Tree;
use crate::team::Beat;
use crate::team::QueueState;
use crate::team::Render;
use crate::tui::image;
use crate::tui::math;
use crate::tui::math::Formula;
#[cfg(test)]
use crate::tui::plan_images;
use crate::tui::status::{Field, StatusData, Workers};
use crate::tui::syntax::{self, Language, State};
use crate::tui::theme::Appearance;
use crate::tui::theme::Theme;

#[cfg(test)]
use super::login::{Choice, Listed, Outcome, Pick};

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
                    lines.push(Line::from(mem::replace(&mut current, row.rest.clone())));
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
                lines.push(Line::from(mem::replace(&mut current, row.rest.clone())));
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
                    lines.push(Line::from(mem::replace(&mut current, row.rest.clone())));
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

/// The character a formula's first reserved row carries until the drawing pass finds it (D165).
///
/// A picture is placed at a row of the transcript, and a formula is not an entry of its own: it is
/// inside a message, whose rows are wrapped and shifted before they are drawn. The marker is how a
/// row that has to be blank in the end survives that, without the picture's position being counted
/// through a wrap that splits other rows. It is a private-use character and is replaced by a blank
/// before anything is drawn or copied.
const FORMULA: char = '\u{E000}';

/// The character the rest of an inline formula's reserved run carries (D165).
///
/// A formula inside a line of text takes cells of that line, and the cells are reserved as a run of
/// characters the wrapper measures like any other text but cannot break: a private-use letter is
/// one cell wide and is not whitespace, so the run moves to the next row whole rather than being
/// split down the middle. Both characters are replaced by blanks before anything is drawn.
const GUTTER: char = '\u{E001}';

/// The most rows a formula may take. Beyond this the source is drawn instead: a page-high equation
/// is not what a transcript is for (D165).
const MAX_MATH_ROWS: u16 = 16;

/// The Markdown as it was written, with the other spellings of a formula rewritten into the parser's
/// own (D165).
///
/// `pulldown-cmark` knows `$x$` and `$$x$$` and nothing else. The two spellings people write instead
/// reach it as ordinary text, and have to: a dollar with a space inside is what money looks like, and
/// the backslash of `\(x\)` is an escape that is gone before any maths layer could see it. So they
/// are rewritten here, before it parses anything — outside code, which is what a person wrote to be
/// read exactly as it is: a fenced block, or a span between backticks.
fn delimiters(text: &str) -> Cow<'_, str> {
    if !text.contains('$') && !text.contains("\\(") && !text.contains("\\[") {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut changed = false;
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A fence line is a whole line, and is copied as one: read as an inline span instead, its
        // backticks would open a span that never closes and the rest of the message would be left
        // exactly as it was written (D165).
        if (i == 0 || chars[i - 1] == '\n')
            && let Some(marker) = fence_marker(&chars[i..])
        {
            match fence {
                Some((open, count)) if open == marker.0 && marker.1 >= count => fence = None,
                None => fence = Some(marker),
                _ => {}
            }
            let end = chars[i..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(chars.len(), |at| i + at);
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        if fence.is_some() {
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            // A span in backticks is code however it reads, so it is copied to its closing run —
            // and a run that never closes is just a backtick, which is what the parser makes of it.
            '`' => {
                let run = chars[i..].iter().take_while(|c| **c == '`').count();
                match code_span_end(&chars, i + run, run) {
                    Some(end) => {
                        out.extend(&chars[i..end]);
                        i = end;
                    }
                    None => {
                        out.extend(&chars[i..i + run]);
                        i += run;
                    }
                }
            }
            // TeX's own delimiters: the backslash is what makes them unambiguous.
            '\\' if matches!(chars.get(i + 1), Some('(' | '[')) => {
                let (close, mark) = match chars[i + 1] {
                    '(' => (')', "$"),
                    _ => (']', "$$"),
                };
                let mut j = i + 2;
                let mut end = None;
                while j + 1 < chars.len() {
                    if chars[j] == '\\' && chars[j + 1] == close {
                        end = Some(j);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(j) if j > i + 2 => {
                        let body: String = chars[i + 2..j].iter().collect();
                        out.push_str(mark);
                        out.push_str(body.trim());
                        out.push_str(mark);
                        changed = true;
                        i = j + 2;
                    }
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            // A dollar with a space inside: maths when it reads as maths, money when it does not.
            '$' if chars.get(i + 1) != Some(&'$') => {
                let mut j = i + 1;
                let mut end = None;
                while j < chars.len() {
                    match chars[j] {
                        '\n' => break,
                        '\\' => j += 2,
                        '$' => {
                            end = Some(j);
                            break;
                        }
                        _ => j += 1,
                    }
                }
                match end {
                    Some(j) if maths_like(&chars[i + 1..j]) => {
                        let body: String = chars[i + 1..j].iter().collect();
                        let trimmed = body.trim();
                        if trimmed.len() != body.len() {
                            out.push('$');
                            out.push_str(trimmed);
                            out.push('$');
                            changed = true;
                        } else {
                            out.extend(&chars[i..=j]);
                        }
                        i = j + 1;
                    }
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    match changed {
        true => Cow::Owned(out),
        false => Cow::Borrowed(text),
    }
}

/// Just past the run of `run` backticks that closes a code span beginning at `from`, if one does.
fn code_span_end(chars: &[char], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        let here = chars[j..].iter().take_while(|c| **c == '`').count();
        if here == 0 {
            j += 1;
            continue;
        }
        if here == run {
            return Some(j + here);
        }
        j += here;
    }
    None
}

/// The fence a line opens or closes, when it starts with one: three or more backticks or tildes, no
/// more than three spaces in.
fn fence_marker(line: &[char]) -> Option<(char, usize)> {
    let indent = line.iter().take_while(|c| **c == ' ').count();
    if indent > 3 {
        return None;
    }
    let marker = *line.get(indent)?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let count = line[indent..].iter().take_while(|c| **c == marker).count();
    (count >= 3).then_some((marker, count))
}

/// Whether what stands between two dollars is a formula rather than an amount of money (D165).
///
/// A formula has TeX in it — a command, a script, a group, a relation — or else it is one word of
/// symbols; money has figures in it, and prose has spaces.
fn maths_like(body: &[char]) -> bool {
    let body: String = body.iter().collect();
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains(['\\', '^', '_', '{', '}', '=', '+', '<', '>', '|']) {
        return true;
    }
    !trimmed.contains([' ', '\t', ',']) && !trimmed.chars().any(|c| c.is_ascii_digit())
}

/// Markdown to rows, fitted to `room` columns. Soft breaks are kept as line breaks, since a model's
/// line breaks are usually deliberate (tables, lists written by hand).
///
/// The room is what a table needs and nothing else does: a table is the one construct whose layout
/// cannot survive being wrapped afterwards, because a wrapped row loses the alignment that makes
/// it a table. Every other block is wrapped by [`wrap`] as it always was.
pub fn markdown(text: &str, theme: &Theme, room: usize) -> Vec<Row> {
    markdown_over(text, theme, Style::new(), room, false, 1.0, &mut Vec::new())
}

/// Markdown drawn over `base`, which is what a message the user typed gets: the text is already in
/// their colour, and the emphasis they wrote is a modifier on top of it.
fn markdown_over(
    text: &str,
    theme: &Theme,
    base: Style,
    room: usize,
    pictures: bool,
    scale: f32,
    images: &mut Vec<Formula>,
) -> Vec<Row> {
    let mut md = Markdown {
        theme: theme.clone(),
        base,
        room,
        pictures,
        scale,
        ..Markdown::default()
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        // `$…$` and `$$…$$` arrive as their own events, which is what makes a formula a picture
        // rather than a span of text (D165).
        | Options::ENABLE_MATH;
    for event in Parser::new_ext(&delimiters(text), options) {
        md.event(event);
    }
    md.flush();
    images.append(&mut md.images);
    md.rows
}

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
    /// Whether a formula can be drawn at all: a terminal with no graphics protocol gets its source
    /// (D59, D165).
    pictures: bool,
    /// The formulas this block reserved rows for, in the order their markers appear.
    images: Vec<Formula>,
    /// A fence tagged `typst-math` while its body is being collected: what is between the fences is
    /// a formula in Typst's own notation, and it is read as maths rather than as markdown (D165).
    math_block: Option<String>,
    /// `[ui] math-scale`, which is 1.0 unless someone asked for larger or smaller formulas (D165).
    scale: f32,
    /// Whether the last row pushed is the blank a block break asked for (D165).
    block_blank: bool,
}

impl Default for Markdown {
    fn default() -> Self {
        Self {
            room: 0,
            theme: Theme::default(),
            rows: Vec::new(),
            line: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            bullet: None,
            quote: 0,
            code: false,
            lang: Language::default(),
            code_state: State::default(),
            blank_before_next_block: false,
            link: None,
            table: None,
            base: Style::default(),
            pictures: false,
            images: Vec::new(),
            math_block: None,
            // Not a type's zero: the size the fonts and the cell work out to is the default, and
            // zero would draw a formula half-size (D165).
            scale: 1.0,
            block_blank: false,
        }
    }
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
        self.block_blank = false;
        let (first, rest) = self.prefixes();
        let spans = mem::take(&mut self.line);
        self.rows.push(Row { first, rest, spans });
    }

    fn start_block(&mut self) {
        self.flush();
        let blank = self.blank_before_next_block && !self.rows.is_empty() && self.lists.is_empty();
        if blank {
            self.rows.push(Row::default());
        }
        // Which row, if any, the break put there: a formula takes the break without it.
        self.block_blank = blank;
        self.blank_before_next_block = false;
    }

    /// Typesets one display formula and reserves the rows it will be drawn in (D165).
    ///
    /// The rows are blank in the end, and the first of them carries a marker: the picture is placed
    /// at that row, so the layout and the drawing pass agree even though the rows around it are
    /// wrapped and counted later.
    fn formula(&mut self, source: &str, notation: math::Notation) {
        // A block break puts a blank row above the block, and a picture's ink starts at the very top
        // of its box: that row reads as a hole above a formula, where above a paragraph of text its
        // own line leading fills the same gap. A formula brings its own room — the padding inside
        // the picture — so it takes the break without the blank row (D165).
        self.flush();
        if self.block_blank {
            self.rows.pop();
            self.block_blank = false;
        }
        let room = math::Room {
            cell: image::cell_px(),
            cols: self.room.max(1) as u16,
            rows: MAX_MATH_ROWS,
            colour: maths_colour(&self.theme),
            scale: self.scale,
        };
        let formula = if self.pictures {
            match notation {
                // One reading, decided by how the formula was invoked: a `$…$` body is LaTeX and a
                // fence tagged `typst-math` is Typst's own maths (D165).
                math::Notation::Latex => math::render(source, &room),
                math::Notation::Typst => math::render_typst(source, &room),
            }
        } else {
            None
        };
        let Some(formula) = formula else {
            // The rule that keeps the transcript honest: what cannot be typeset is drawn as the
            // source, never as something wrong (D165). A formula written over several lines keeps
            // them: a line break is a space to TeX, but the shape a writer gives a long formula is
            // what makes its source readable, and a row of text holds no line break at all —
            // `$$…$$` written on three lines came out on one, which is how a real transcript
            // showed it.
            //
            // LaTeX's source is marked with the delimiters it was written in, so it is read back as
            // a formula where it is pasted; a fence's body is shown as the block of code it is,
            // since a fence is what says "Typst" and the copy keeps the fence (D166).
            let is_latex = notation == math::Notation::Latex;
            let style = if is_latex {
                self.code_style()
            } else {
                self.block_style()
            };
            let body = match is_latex {
                true => source,
                false => source.trim_end_matches(['\n', '\r']),
            };
            let lines: Vec<&str> = body
                .split('\n')
                .map(|line| line.trim_end_matches('\r'))
                .collect();
            for (i, line) in lines.iter().enumerate() {
                if i == 0 && is_latex {
                    self.line.push(Span::styled("$$", style));
                }
                if !is_latex {
                    self.line.push(Span::styled("  ".to_owned(), style));
                }
                self.line.push(Span::styled((*line).to_owned(), style));
                if i + 1 == lines.len() && is_latex {
                    self.line.push(Span::styled("$$", style));
                }
                self.flush();
            }
            self.blank_before_next_block = true;
            return;
        };
        // The rows this formula is drawn in are the rows the layout reserves (D165): the picture is
        // scaled into them, so the two cannot disagree about how tall it is.
        let rows = formula.cells.1.max(1);
        self.rows
            .push(Row::plain(vec![Span::raw(FORMULA.to_string())]));
        for _ in 1..rows {
            self.rows.push(Row::default());
        }
        self.images.push(formula);
        // The same reason below: the room inside the picture is the separation, so the text that
        // follows a formula starts on the next row instead of after a blank one (D165).
        self.blank_before_next_block = false;
    }

    /// Typesets one formula inside a line of text and reserves the cells it is drawn in (D165).
    ///
    /// It cannot have rows of its own, so what it gets instead is cells of the line it is in: a run
    /// the wrapper measures as usual and cannot break inside. The run moves to the next row with
    /// the words around it, and the drawing pass finds the marker and puts the picture at the column
    /// the run ended up at.
    fn inline(&mut self, source: &str) {
        // A table's cells are fitted rather than wrapped, so there is no column for a picture to
        // take: a formula inside one stays its source, as it does in a terminal with no pictures.
        let formula = if self.table.is_some() || !self.pictures {
            None
        } else {
            let room = math::Room {
                cell: image::cell_px(),
                cols: self.room.max(1) as u16,
                rows: 1,
                colour: maths_colour(&self.theme),
                scale: self.scale,
            };
            math::render_inline(source, &room)
        };
        let Some(formula) = formula else {
            let style = self.code_style();
            // A formula inside a line has no rows of its own, so a break in its source — a
            // paragraph's soft break inside `$…$` — is written as the space TeX reads there.
            self.text(&format!("${}$", source.replace(['\n', '\r'], " ")), style);
            return;
        };
        let mut reserved = String::with_capacity(formula.cells.0.max(1) as usize);
        reserved.push(FORMULA);
        for _ in 1..formula.cells.0.max(1) {
            reserved.push(GUTTER);
        }
        self.line.push(Span::raw(reserved));
        self.images.push(formula);
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
            // A fence tagged `typst-math` holds a formula, and its body is taken as it is written:
            // markdown inside maths would be read as emphasis, and a formula is not prose (D165).
            Event::Text(t) if self.math_block.is_some() => {
                if let Some(body) = &mut self.math_block {
                    body.push_str(&t);
                }
            }
            Event::Text(t) => {
                let style = self.style();
                self.text(&t, style);
            }
            Event::Code(t) => self.text(&t, self.code_style()),
            // A display formula is typeset and placed as a picture, and so is one inside a line:
            // both are drawn in the cells the layout reserves for them (D165).
            Event::DisplayMath(t) => self.formula(&t, math::Notation::Latex),
            Event::InlineMath(t) => self.inline(&t),
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
            Event::Html(t) | Event::InlineHtml(t) => {
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
                let lang = match kind {
                    CodeBlockKind::Fenced(tag) => Language::parse(&tag),
                    // An indented block names no language, so there is nothing to look up.
                    CodeBlockKind::Indented => Language::Plain,
                };
                self.start_block();
                // The one fence tag that is not code: Typst's own maths, which is a formula rather
                // than the highlighted text a language tag gives (D165). `typst` itself stays code,
                // because a tag that meant both would leave the same body read as maths when it
                // happens to parse and as code when it does not.
                if matches!(&lang, Language::Tagged(tag) if tag == math::TYPST) {
                    self.math_block = Some(String::new());
                } else {
                    self.code = true;
                    self.lang = lang;
                    self.code_state = State::default();
                }
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
            TagEnd::CodeBlock => match self.math_block.take() {
                Some(body) => self.formula(&body, math::Notation::Typst),
                None => {
                    self.code = false;
                    self.lang = Language::Plain;
                    self.code_state = State::default();
                    self.flush();
                    self.blank_before_next_block = true;
                }
            },
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
    /// True when the frame's activity line already names the call in flight, so a tool block that
    /// is still running does not lead with a second spinner (D172).
    activity: bool,
    /// The transcript width, so an image's reserved rows match what is drawn (D59).
    room: (u16, u16),
    /// Whether the terminal can draw a picture, which decides whether an attachment gets rows
    /// reserved for one (D59).
    images: bool,
    /// `[ui] math-scale`: a formula drawn larger or smaller than the fonts and the cell work out to
    /// (D165).
    scale: f32,
}

fn thinking_rows(text: &str, shown: bool, theme: &Theme, room: usize) -> Vec<Row> {
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
    // D151: summaries contain Markdown too. Preserve list/code prefixes inside the thinking bar.
    let mut rows = markdown_over(
        text,
        theme,
        bar.add_modifier(Modifier::ITALIC),
        room.saturating_sub(2),
        false,
        1.0,
        &mut Vec::new(),
    );
    for row in &mut rows {
        row.first.insert(0, Span::styled("│ ", bar));
        row.rest.insert(0, Span::styled("│ ", bar));
    }
    rows
}

/// What the user typed, read as markdown in their own colour (D81).
///
/// This is what pi does — its user-message component is its markdown renderer with the user's
/// colour as the base and the user-message background behind it — so a prompt and a reply are read
/// by the same code. The text is not bold, so that `**bold**` inside it is visible; the `>` marker is
/// what says whose message it is. Every row gets the marker's width as its continuation, including
/// a row a block construct made, so a list in a prompt still lines up under the message.
fn user_rows(
    text: &str,
    theme: &Theme,
    room: usize,
    pictures: bool,
    scale: f32,
    images: &mut Vec<Formula>,
) -> Vec<Row> {
    let mut rows = markdown_over(
        text,
        theme,
        Style::new().fg(theme.user_message_text),
        room,
        pictures,
        scale,
        images,
    );
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
/// One entry, drawn as the transcript draws it: a tool block with its spinner and diff, a
/// compaction, or wrapped markdown rows. Shared by the lead's transcript and a worker's view, so
/// entering a worker shows the same rendering rather than a plain tail (D195).
fn entry_render(
    entry: &Entry,
    view: View,
    theme: &Theme,
    now: Instant,
) -> (Vec<Line<'static>>, Vec<Formula>) {
    let width = view.room.0 as usize;
    match entry {
        Entry::Tool {
            tasks,
            name,
            detail,
            result,
            diff,
            timeout,
            spent,
            ..
        } => {
            let elapsed = spent.map(|spent| tasks_elapsed_text(spent.elapsed(now)));
            let (mut rows, mut bg) = tool_block(
                name,
                detail,
                result.as_ref(),
                diff.as_deref(),
                &view,
                Timing {
                    elapsed: elapsed.as_deref(),
                    limit: *timeout,
                },
                theme,
            );
            background_block(&mut rows, &mut bg, tasks, view.spinner, now, theme);
            (shaded(&rows, bg, width), Vec::new())
        }
        Entry::Compaction(v) => {
            let (rows, bg) =
                compaction_block(v, view.thinking, view.expand, view.spinner, theme, width);
            (shaded(&rows, bg, width), Vec::new())
        }
        entry => {
            let mut formulas = Vec::new();
            let rows = entry_rows(entry, view, theme, &mut formulas);
            (rows.iter().flat_map(|r| wrap(r, width)).collect(), formulas)
        }
    }
}

fn entry_rows(entry: &Entry, view: View, theme: &Theme, images: &mut Vec<Formula>) -> Vec<Row> {
    match entry {
        Entry::Thinking(text) => thinking_rows(text, view.thinking, theme, view.room.0 as usize),
        Entry::User(text) => user_rows(
            text,
            theme,
            view.room.0 as usize,
            view.images,
            view.scale,
            images,
        ),
        Entry::Assistant(text) => markdown_over(
            text,
            theme,
            Style::new(),
            view.room.0 as usize,
            view.images,
            view.scale,
            images,
        ),
        Entry::Image(image) => {
            // The picture is drawn over the rows after this one, so they are left empty for it. The
            // marker line stays, and is all a terminal without a graphics protocol shows (D59).
            let mut out: Vec<Row> = vec![Row::plain(vec![Span::styled(
                image::fallback(&image.name, image.width, image.height),
                dim(theme),
            )])];
            if view.images {
                // The rows the picture will cover, from its own size scaled into the transcript
                // width: kitty draws it at its pixel size when it fits, and scaled when it does
                // not (D59).
                let fit = image::fit((image.width, image.height), (view.room.0, image::MAX_ROWS));
                out.extend(iter::repeat_n(Row::plain(Vec::new()), fit.rows as usize));
            }
            out
        }
        Entry::Note(kind, text) => {
            let style = match kind {
                NoteKind::Info => dim(theme),
                NoteKind::Plugin => Style::new().fg(theme.tool_output),
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
/// hidden with ctrl+t) and a preview of the summary, then what was kept. The summary is collapsed
/// the way a tool's output is (D124): while it streams the last few lines are shown, because the
/// new text is at the end, and once it is done the first few, because the beginning is its gist.
fn compaction_block(
    v: &CompactionView,
    thinking: bool,
    expand: bool,
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
        // One control for the entry: ctrl+o opens the compaction, and the reasoning it was written
        // from is part of what it opens. It used to follow ctrl+t instead, which meant a person who
        // had reasoning on saw every compaction arrive with all of its thinking laid out — two
        // toggles for one entry, and the default was the wrong way round (D124).
        if expand && thinking {
            rows.extend(thinking_rows(
                reasoning,
                true,
                theme,
                room.saturating_sub(2),
            ));
        } else {
            let hint = if thinking {
                "ctrl+o shows all"
            } else {
                "ctrl+t shows it"
            };
            rows.push(Row::plain(vec![Span::styled(
                format!(
                    "∴ reasoned for {} lines · {hint}",
                    reasoning.lines().count()
                ),
                italic,
            )]));
        }
    }
    if !v.summary.trim().is_empty() {
        rows.push(Row::default());
        let lines: Vec<&str> = v.summary.trim_end().lines().collect();
        let running = matches!(v.state, CompactState::Running);
        if expand || lines.len() <= PREVIEW_LINES {
            rows.extend(markdown(&v.summary, theme, room));
        } else {
            let note = muted.add_modifier(Modifier::ITALIC);
            let (shown, hidden) = if running {
                let hidden = lines.len() - PREVIEW_LINES;
                (
                    lines[hidden..].join("\n"),
                    format!("… {hidden} earlier lines"),
                )
            } else {
                (
                    lines[..PREVIEW_LINES].join("\n"),
                    format!("… {} more lines", lines.len() - PREVIEW_LINES),
                )
            };
            rows.extend(markdown(&shown, theme, room));
            rows.push(Row::plain(vec![Span::styled(
                format!("{hidden} · ctrl+o shows all"),
                note,
            )]));
        }
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

/// background execution is host state, not a successful launch result or guest text (D159).
fn background_block(
    rows: &mut [Row],
    bg: &mut Color,
    tasks: &[TaskView],
    spinner: usize,
    now: Instant,
    theme: &Theme,
) {
    if tasks.is_empty() {
        return;
    }
    let running = tasks.iter().any(|task| task.state == TaskState::Running);
    let unknown = tasks
        .iter()
        .any(|task| task.state == TaskState::Unavailable);
    *bg = if running || unknown {
        theme.tool_pending_bg
    } else if tasks.iter().all(|task| task.state == TaskState::Succeeded) {
        theme.tool_success_bg
    } else {
        theme.tool_error_bg
    };
    if let Some(row) = rows.first_mut() {
        if running {
            row.spans.insert(
                0,
                Span::styled(
                    format!("{} ", SPINNER[spinner % SPINNER.len()]),
                    Style::new().fg(theme.warning),
                ),
            );
        }
        for task in tasks {
            // Each task is timed from when it started, not from when the interface drew it (D172).
            row.spans.push(Span::styled(
                format!(
                    "  [{}: {} · {}]",
                    task.id,
                    task.state.label(),
                    tasks_elapsed_text(task.elapsed(now))
                ),
                dim(theme),
            ));
        }
    }
}

/// A tool call as a shaded block: the call, then its output. Collapsed, bash shows its last
/// lines (where errors and test summaries are) and other tools their first.
/// How long a call has been going, or took once it answered (D172), and the deadline it was given
/// when the model set one (D152).
#[derive(Clone, Copy, Default)]
struct Timing<'a> {
    elapsed: Option<&'a str>,
    limit: Option<u64>,
}

impl Timing<'_> {
    /// The words after the command: `· 5.0s`, `· 5.0s of 30s`, or `· 30s limit` where there is no
    /// stopwatch to put the limit after.
    fn words(&self) -> String {
        let mut words = String::new();
        if let Some(elapsed) = self.elapsed {
            words.push_str(&format!(" · {elapsed}"));
        }
        if let Some(seconds) = self.limit {
            if self.elapsed.is_some() {
                words.push_str(&format!(" of {seconds}s"));
            } else {
                words.push_str(&format!(" · {seconds}s limit"));
            }
        }
        words
    }
}

fn tool_block(
    name: &str,
    detail: &str,
    result: Option<&(bool, String)>,
    diff: Option<&str>,
    view: &View,
    timing: Timing<'_>,
    theme: &Theme,
) -> (Vec<Row>, Color) {
    let (expand, spinner) = (view.expand, view.spinner);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    // The tool's own name has a role of its own, so a theme can quiet the titles without touching
    // the accent everywhere else (D72).
    let accent = Style::new()
        .fg(theme.tool_title)
        .add_modifier(Modifier::BOLD);
    let mut detail_lines = detail.lines();
    let first = detail_lines.next().unwrap_or_default().to_owned();
    let mut header = Vec::new();
    if result.is_none() && !view.activity {
        header.push(Span::styled(
            format!("{} ", SPINNER[spinner % SPINNER.len()]),
            Style::new().fg(theme.warning),
        ));
    }
    if name == "bash" && !first.is_empty() {
        header.push(Span::styled("$ ", accent));
        header.push(Span::styled(first, bold));
    } else {
        header.push(Span::styled(name.to_owned(), accent));
        if !first.is_empty() {
            header.push(Span::raw(" "));
            header.push(Span::styled(first, bold));
        }
    }
    // A stopwatch beside the call — running while it works, the total once it has answered, so a
    // command that took a minute says so afterwards (D172) — and the deadline the call set when it
    // set one (D152).
    let tail = timing.words();
    if !tail.is_empty() {
        header.push(Span::styled(tail, dim(theme)));
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
/// One picture the transcript drew, and where it goes.
struct At {
    /// The line the picture's box starts at: for a picture its own text announces, the row below the
    /// line announcing it; for a formula, the row below its marker, or the marker's own row when the
    /// formula sits inside a line (D165).
    line: usize,
    /// The column its first cell is at, or `None` when it starts its row. A formula inside a line of
    /// text starts where the words before it leave off (D165).
    column: Option<u16>,
    /// The picture, and the cells it is drawn in. An attachment states its own size; a formula's
    /// cells are the rows the layout reserved for it (D165).
    picture: Picture,
    /// The formula as it was written, which is what a copy of it takes (D166). Empty for a picture
    /// an attachment brings, which has no source in the conversation.
    tex: String,
    /// Whether the formula sits inside a line: `$…$` when it is copied, `$$…$$` when it is a block.
    inline: bool,
    /// Whether it was written in Typst's own notation, in which case a copy carries the fence that
    /// asks for it rather than TeX's delimiters (D166).
    typst: bool,
    /// The id the terminal knows it by: the entry's position gives it, so the same picture keeps
    /// the same id every frame and does not have to be re-sent (D59).
    id: u32,
}

/// A picture the transcript places, and its size in cells.
struct Picture {
    image: Image,
    cells: image::Fit,
}

fn transcript_with_images(
    app: &mut App,
    width: u16,
    activity: bool,
) -> (Vec<Line<'static>>, Vec<At>) {
    let mut images: Vec<At> = Vec::new();
    let mut lines = Vec::new();
    for i in 0..app.entries.len() {
        let running = match &app.entries[i] {
            Entry::Tool { result, tasks, .. } => {
                result.is_none() || tasks.iter().any(|task| task.state == TaskState::Running)
            }
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
                activity,
                room: (width, image::MAX_ROWS),
                images: app.protocol != image::Protocol::Text,
                scale: app.header.math_scale,
            };
            let (rendered, formulas) = entry_render(&app.entries[i], view, &app.theme, app.now);
            app.cache[i] = Some(Cached {
                width,
                expand: app.expand_tools,
                thinking: app.show_thinking,
                lines: rendered,
                pictures: formulas,
            });
        }
        if i > 0 {
            lines.push(Line::default());
        }
        // The marker line is where the picture goes, so its index is taken before the entry's own
        // lines are added and the picture starts there.
        if let Some(image) = app.cached_image(i) {
            let cells = image::fit((image.width, image.height), (width, image::MAX_ROWS));
            images.push(At {
                // The picture goes under the line that announces it.
                line: lines.len() + 1,
                column: None,
                picture: Picture { image, cells },
                // An attachment has no source in the conversation: it *is* the content.
                tex: String::new(),
                inline: false,
                typst: false,
                id: picture_id(i, 0),
            });
        }
        let start = lines.len();
        lines.extend(
            app.cache[i]
                .as_ref()
                .map(|c| c.lines.iter().cloned())
                .into_iter()
                .flatten(),
        );
        // Every formula left a marker where it is drawn: a row of its own for a block, and a run of
        // cells inside a row for one that sits in a line. What is drawn is the picture; the cells it
        // took are blanked so nothing shows through it (D165).
        let pictures = app
            .cache
            .get(i)
            .map(|c| c.as_ref().map(|c| c.pictures.clone()).unwrap_or_default())
            .unwrap_or_default();
        for (k, image) in pictures.into_iter().enumerate() {
            let Some(at) = lines[start..]
                .iter()
                .position(|line| plain(line).contains(FORMULA))
            else {
                // The block was cached without its picture, which means the source is what is on
                // screen; there is nothing to place.
                continue;
            };
            let (line, column) = if image.inline {
                let Some(column) = unmark(&mut lines[start + at]) else {
                    continue;
                };
                (start + at, Some(column as u16))
            } else {
                // A block's marker has the row to itself, and the picture starts on the row below.
                lines[start + at] = Line::default();
                (start + at + 1, None)
            };
            images.push(At {
                line,
                column,
                tex: image.source.clone(),
                inline: image.inline,
                typst: image.notation == math::Notation::Typst,
                // The cells the formula says it occupies, asked for as a box: the terminal scales
                // the picture into them, whatever its own pixels are (D165).
                picture: Picture {
                    cells: image::Fit {
                        cols: image.cells.0.max(1),
                        rows: image.cells.1.max(1),
                        scaled: true,
                    },
                    image: image.image,
                },
                id: picture_id(i, k + 1),
            });
        }
    }
    (lines, images)
}

/// The terminal id of one of an entry's pictures (D59, D165).
///
/// An id has to be the same in every frame and different for every picture on the screen: the entry
/// it belongs to and its place in that entry give both, and the room for 63 pictures in a message
/// is far beyond what one reply holds.
fn picture_id(entry: usize, within: usize) -> u32 {
    (entry as u32) * 64 + within as u32 + 1
}

/// The colour a formula is drawn in: the theme's text colour, or near-black or near-white when the
/// theme leaves it to the terminal, which a picture cannot do (D165).
fn maths_colour(theme: &Theme) -> (u8, u8, u8) {
    match theme.text {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => match theme.appearance {
            Appearance::Light => (24, 24, 24),
            Appearance::Dark => (232, 232, 232),
        },
    }
}

/// Blanks the run a formula reserved inside a line, returning the column it started at (D165).
///
/// The run is the marker and the fillers after it, one cell each: spaces keep the line's layout
/// exactly, so what the row shows at that column is the picture and nothing else.
fn unmark(line: &mut Line<'static>) -> Option<usize> {
    let mut column = 0;
    for span in &mut line.spans {
        let Some(at) = span.content.find(FORMULA) else {
            column += span.content.width();
            continue;
        };
        let before = &span.content[..at];
        let column = column + before.width();
        let chars: Vec<char> = span.content.chars().collect();
        let from = before.chars().count();
        let mut to = from;
        while to < chars.len() && (chars[to] == FORMULA || chars[to] == GUTTER) {
            to += 1;
        }
        let mut blanked: String = chars[..from].iter().collect();
        blanked.extend(iter::repeat_n(' ', to - from));
        blanked.extend(chars[to..].iter());
        *span.content.to_mut() = blanked;
        return Some(column);
    }
    None
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
    for span in mem::take(&mut line.spans) {
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
    let text = mem::take(run);
    out.push(match selected {
        Some(true) => Span::styled(text, style.patch(selected_style(theme))),
        _ => Span::styled(text, style),
    });
}

pub(super) fn home_relative(path: &Path) -> String {
    let shown = path.display().to_string();
    match env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

fn draw_panels(frame: &mut Frame<'_>, panels: &[PanelView], area: Rect, theme: &Theme) {
    let mut y = area.y;
    for panel in panels {
        if y >= area.bottom() {
            break;
        }
        let height = (panel.rows.len() as u16)
            .saturating_add(2)
            .min(area.bottom() - y);
        let room = area.width.saturating_sub(4) as usize;
        let lines: Vec<Line> = panel
            .rows
            .iter()
            .map(|row| match row {
                PanelRow::Text(spans) => Line::from(
                    spans
                        .iter()
                        .map(|span| {
                            let style = match span.tone {
                                Tone::Text => Style::new().fg(theme.tool_output),
                                Tone::Muted => dim(theme),
                                Tone::Success => Style::new().fg(theme.success),
                                Tone::Warning => Style::new().fg(theme.warning),
                                Tone::Error => Style::new().fg(theme.error),
                                Tone::Emphasis => {
                                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
                                }
                            };
                            Span::styled(span.text.clone(), style)
                        })
                        .collect::<Vec<_>>(),
                ),
                PanelRow::Progress {
                    completed,
                    total,
                    label,
                } => {
                    let width = room.saturating_sub(label.width().saturating_add(3));
                    let filled = if *total == 0 {
                        0
                    } else {
                        ((*completed as u128 * width as u128) / *total as u128).min(width as u128)
                            as usize
                    };
                    Line::from(vec![
                        Span::raw(format!("{label} [")),
                        Span::styled("━".repeat(filled), Style::new().fg(theme.accent)),
                        Span::styled("─".repeat(width.saturating_sub(filled)), dim(theme)),
                        Span::raw("]"),
                    ])
                }
            })
            .collect();
        let block = Block::bordered()
            .title(format!("{} · {}", panel.plugin, panel.title))
            .border_style(Style::new().fg(theme.border_muted));
        frame.render_widget(
            Paragraph::new(lines).block(block),
            Rect::new(area.x, y, area.width, height),
        );
        y = y.saturating_add(height);
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // Filled in as the frame is drawn: a menu or dialog that covers a picture keeps it off the
    // screen (D161).
    app.covered.clear();
    let inner_width = area.width.saturating_sub(2) as usize;
    let layout = app.editor.layout(inner_width.max(1));
    let editor_rows = layout.rows.len().clamp(1, MAX_EDITOR_ROWS);
    // What is waiting to be delivered, the sooner one first: a steer enters at the next request,
    // a queued prompt only after the turn (D149).
    let queue_rows = (app.steering.len() + app.jobs.len()).min(3);
    let panel_rows = app
        .plugin_panels
        .iter()
        .map(|panel| panel.rows.len() + 2)
        .sum::<usize>()
        .min(area.height as usize / 3);
    let [
        transcript_area,
        panels_area,
        queue_area,
        activity_area,
        editor_area,
        status_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(panel_rows as u16),
        Constraint::Length(queue_rows as u16),
        Constraint::Length(1),
        Constraint::Length(editor_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);

    if let Some(name) = &app.worker
        && let Some(worker) = app.team.iter().find(|worker| &worker.name == name).cloned()
    {
        draw_worker(frame, &worker, app, transcript_area);
    } else {
        // This transcript is the one the frame's activity line speaks for, so a call in flight is
        // named there and does not spin in its own block as well (D172).
        draw_transcript(frame, app, transcript_area, true);
    }
    draw_panels(frame, &app.plugin_panels, panels_area, &app.theme);

    let pending = |label: &'static str, text: &String| {
        let first = text.lines().next().unwrap_or_default();
        Line::from(vec![
            Span::styled(label, dim(&app.theme)),
            Span::raw(first.to_owned()),
        ])
    };
    let mut queued: Vec<Line> = app
        .steering
        .iter()
        .take(queue_rows)
        .map(|q| pending("steering › ", q))
        .collect();
    queued.extend(
        app.queue
            .iter()
            .take(queue_rows - queued.len())
            .map(|q| pending("queued › ", q)),
    );
    frame.render_widget(Paragraph::new(queued), queue_area);

    // A question is answered where it is drawn, at the end of the transcript, so the line that is
    // always visible carries its keys and says when it has been scrolled out of sight (D197).
    let activity = match &app.ask {
        // The question is the transcript's last rows, so it can be scrolled away: then the line
        // says where it is rather than listing keys for a block that is not on the screen (D197).
        Some(dialog) if app.scroll > 0 => Line::from(vec![
            Span::styled("question below ", Style::new().fg(app.theme.accent)),
            Span::styled("· end to see it", dim(&app.theme)),
        ]),
        Some(dialog) => Line::from(vec![
            Span::styled("question ", Style::new().fg(app.theme.accent)),
            Span::styled(dialog.help(), dim(&app.theme)),
        ]),
        None => match (&app.busy, &app.stopped) {
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
    // While a question is open the prompt keeps its text but not the cursor: what is typed goes to
    // the note or the answer, which draws its own (D197).
    if app.ask.is_none() {
        frame.set_cursor_position(Position::new(
            editor_area.x + 1 + cursor_col as u16,
            editor_area.y + 1 + (cursor_row - top) as u16,
        ));
    }

    // The line is whatever `[ui] status` says, filled from the session (D36).
    let right = app.header.status.render(&StatusData {
        provider: &app.header.provider,
        model: &app.header.model,
        usage: app.usage,
        cost: app.cost,
        priced: app.header.priced,
        quota: app.quota.as_ref(),
        running_tasks: app.running_tasks,
        // Finished workers keep their conversation but are not agents at work (D195).
        workers: Workers {
            live: app.team.iter().filter(|worker| !worker.finished).count(),
            active: app
                .team
                .iter()
                .filter(|worker| !worker.finished && worker.state == "running")
                .count(),
        },
        context: app.context,
        window: app.header.context_window,
        workspace: &home_relative(&app.header.workspace),
        // A refill is counted down from the moment the frame is drawn (D49).
        now: SystemTime::now(),
    });
    // The workspace is drawn on the left unless the template already places it, so a user who
    // moves it into the line does not see it twice (D36).
    if !app.header.status.contains(Field::Workspace) {
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
    if let Some(selected) = app.overview {
        surface = Some(draw_overview(frame, app, selected, area));
    }
    if let Some(picker) = &app.resume {
        surface = Some(draw_resume(frame, picker, area, &app.theme));
    }
    if let Some(tree) = &mut app.tree {
        surface = Some(draw_tree(frame, tree, area, &app.theme));
    }
    if let Some(confirm) = &app.confirm {
        surface = Some(draw_confirm(frame, confirm, area, &app.theme));
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
    if app.dialog_seen.is_some() {
        // A modal owns the screen, so no picture is drawn while it is open (D161). A question is
        // the transcript's own last rows, so the pictures above it stay where they are (D197).
        app.covered.push(area);
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

/// Asks whether the branch a move in `/tree` leaves behind is worth a summary (D167).
///
/// One keystroke either way, and escape leaves the conversation where it is: the move is the
/// person's decision already, and the summary is the only part of it that spends a request.
/// A tool's question as the last rows of the transcript (D197): the person reads what led to it and
/// scrolls as ever, and the question is simply the end of the conversation rather than a lid over
/// it. Returns the rows, and the cell the cursor belongs on counted from the block's first row.
///
/// A choice's preview has a panel of its own beside the question when the screen is wide enough for
/// both, and goes under the choices when it is not. Either way the block's height belongs to the
/// question and its choices rather than to the choice under the cursor, so walking them moves
/// nothing else on the screen.
fn ask_rows(
    dialog: &AskDialog,
    width: usize,
    room: Option<usize>,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let width = width.max(8);
    let accent = Style::new().fg(theme.accent);
    let question = &dialog.asked.question;
    let has_preview = question
        .choices
        .iter()
        .any(|choice| !choice.preview.trim().is_empty());
    // The rule above the block spans the whole width; the question keeps the left of it and the
    // preview takes a panel on the right, two cells for the rule between them (D197).
    let panel = (has_preview && width >= PREVIEW_SIDE_MIN).then(|| (width * 2 / 5).clamp(20, 48));
    let column_width = panel.map_or(width, |panel| width - panel - 2);
    let preview = dialog
        .choice(dialog.selected)
        .and_then(|row| question.choices.get(row))
        .map(|choice| choice.preview.as_str())
        .unwrap_or_default();

    let mut column: Vec<Line<'static>> = vec![Line::default()];
    for line in wrap_columns(&question.question, column_width.saturating_sub(2), 4) {
        column.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::new().add_modifier(Modifier::BOLD)),
        ]));
    }
    column.push(Line::default());
    let rows = dialog.rows();
    for (index, choice) in question.choices.iter().enumerate() {
        column.extend(choice_rows(
            column_width,
            index + 1,
            &choice.label,
            &choice.description,
            choice.recommended,
            index == dialog.selected,
            theme,
        ));
    }
    if question.allow_other && rows > 0 {
        column.extend(choice_rows(
            column_width,
            question.choices.len() + 1,
            "Type something…",
            "an answer of your own",
            false,
            dialog.selected + 1 == rows,
            theme,
        ));
    }
    // A preview with no panel of its own goes under the choices, and gives way when the block
    // would be taller than the viewport it lands in.
    if panel.is_none() && has_preview {
        let wrapped: Vec<Vec<String>> = question
            .choices
            .iter()
            .map(|choice| {
                if choice.preview.trim().is_empty() {
                    Vec::new()
                } else {
                    wrap_columns(&choice.preview, column_width.saturating_sub(4), usize::MAX)
                }
            })
            .collect();
        let tallest = wrapped.iter().map(Vec::len).max().unwrap_or(0);
        // The blank, the word "preview" and the footer row are the region's own rows.
        let cap = match room {
            Some(room) => room.saturating_sub(column.len() + 4).min(MAX_PREVIEW_ROWS),
            None => MAX_PREVIEW_ROWS,
        };
        let shown = tallest.min(cap);
        if shown > 0 {
            let selected = dialog
                .choice(dialog.selected)
                .and_then(|row| wrapped.get(row));
            column.push(Line::default());
            column.push(Line::from(vec![
                Span::raw("    "),
                Span::styled("preview", dim(theme)),
            ]));
            for index in 0..shown {
                column.push(match selected.and_then(|lines| lines.get(index)) {
                    Some(line) => Line::from(vec![
                        Span::raw("    "),
                        Span::styled(line.clone(), Style::new()),
                    ]),
                    None => Line::default(),
                });
            }
            if tallest > shown {
                let more = selected.map_or(0, |lines| lines.len().saturating_sub(shown));
                let plural = if more == 1 { "" } else { "s" };
                let footer = if more == 0 {
                    String::new()
                } else {
                    format!("… {more} more line{plural}")
                };
                column.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(footer, dim(theme)),
                ]));
            }
        }
    }
    column.push(Line::default());
    let mut note = None;
    let (label, text) = match &dialog.stage {
        AskStage::Choosing => ("", ""),
        AskStage::Noting { text, .. } => ("note › ", text.as_str()),
        AskStage::Typing { text } => ("answer › ", text.as_str()),
    };
    if !matches!(dialog.stage, AskStage::Choosing) {
        // The cursor is where the next character goes, and a line too long to fit keeps its end —
        // which is the end being written. One column short of the edge: a cursor placed past it
        // wraps, and a wrap scrolls the screen out from under a block meant to stay put (D197).
        let room = column_width.saturating_sub(2 + label.width());
        let used = text.width().min(room);
        let col = (2 + label.width() + used).min(column_width.saturating_sub(1));
        note = Some((column.len(), col));
        column.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(label.to_owned(), accent),
            Span::styled(
                keep_tail(text, room),
                Style::new().add_modifier(Modifier::UNDERLINED),
            ),
        ]));
    }
    if let Some(error) = &dialog.error {
        column.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                clip(error, column_width.saturating_sub(2)),
                Style::new().fg(theme.error),
            ),
        ]));
    }

    let mut rule = "── Question ".to_owned();
    rule.push_str(&"─".repeat(width.saturating_sub(rule.width())));
    let mut lines = vec![Line::from(Span::styled(
        rule,
        Style::new().fg(theme.border_accent),
    ))];
    // The cursor counts from the block's first row, and the rule is that row.
    let cursor = note.map(|(row, col)| (row + 1, col));
    match panel {
        Some(panel_width) => {
            let right = preview_panel(preview, panel_width, column.len(), theme);
            for index in 0..column.len() {
                lines.push(with_panel(
                    column.get(index),
                    right.get(index),
                    column_width,
                    theme,
                ));
            }
        }
        None => lines.extend(column),
    }
    (lines, cursor)
}

/// The preview's own panel: a title row, as much of the preview as the question's rows make room
/// for, and the count of what was left out. Its height is the question's, never its own, so a long
/// diff cannot make the block taller than the choices beside it (D197).
fn preview_panel(preview: &str, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut panel = vec![Line::from(Span::styled("preview", dim(theme)))];
    let wrapped = if preview.trim().is_empty() {
        Vec::new()
    } else {
        wrap_columns(preview, width, usize::MAX)
    };
    let room = rows.saturating_sub(1);
    let shown = if wrapped.len() > room {
        room.saturating_sub(1)
    } else {
        wrapped.len()
    };
    for line in wrapped.iter().take(shown) {
        panel.push(Line::from(Span::styled(line.clone(), Style::new())));
    }
    if wrapped.len() > shown {
        let more = wrapped.len() - shown;
        let plural = if more == 1 { "" } else { "s" };
        panel.push(Line::from(Span::styled(
            format!("… {more} more line{plural}"),
            dim(theme),
        )));
    }
    panel
}

/// One row of the question with the preview's panel beside it, and the rule between them.
fn with_panel(
    left: Option<&Line<'static>>,
    right: Option<&Line<'static>>,
    column_width: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = left.map(|line| line.spans.clone()).unwrap_or_default();
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    spans.push(Span::raw(" ".repeat(column_width.saturating_sub(used))));
    spans.push(Span::styled("│ ", dim(theme)));
    if let Some(right) = right {
        spans.extend(right.spans.clone());
    }
    Line::from(spans)
}

/// How many rows of a choice's preview are drawn under the choices at most, however tall the screen
/// (D197).
const MAX_PREVIEW_ROWS: usize = 10;

/// How wide the screen must be before a choice's preview gets a panel of its own beside the
/// question rather than under it (D197).
const PREVIEW_SIDE_MIN: usize = 64;

/// One choice: its number and label, the recommended marker on the one the asker advises, and the
/// description as subtext under it in a quieter colour than the label (D197).
fn choice_rows(
    width: usize,
    number: usize,
    label: &str,
    description: &str,
    recommended: bool,
    selected: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let accent = Style::new().fg(theme.accent);
    let style = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let marker = if recommended { "  (recommended)" } else { "" };
    let budget = width.saturating_sub(4 + marker.width()).max(4);
    let mut spans = vec![
        Span::styled(if selected { "  › " } else { "    " }, accent),
        Span::styled(format!("{number}. {}", clip(label, budget)), style),
    ];
    if recommended {
        spans.push(Span::styled(marker, Style::new().fg(theme.success)));
    }
    let mut lines = vec![Line::from(spans)];
    for line in wrap_columns(description, width.saturating_sub(6), 3) {
        if line.is_empty() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(line, dim(theme)),
        ]));
    }
    lines
}

fn draw_confirm(frame: &mut Frame<'_>, confirm: &Confirm, area: Rect, theme: &Theme) -> Seen {
    let width = area.width.saturating_sub(4).min(84);
    let inner = width.saturating_sub(2) as usize;
    let (what, asked) = confirm.asked();
    let lines = vec![
        Line::from(Span::styled(keep_head(&what, inner), Style::new())),
        Line::from(Span::styled(keep_head(&asked, inner), Style::new())),
        Line::default(),
        Line::from(Span::styled(
            keep_head("y summarize · n do not · esc stay where you are", inner),
            dim(theme),
        )),
    ];
    modal(
        frame,
        area,
        width,
        " Leave this branch ",
        lines,
        None,
        theme,
    )
    .seen
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
    // The menu is drawn over the transcript, so a picture under it would sit on top of it (D161).
    app.covered.push(rect);
}

fn login_title(login: &Login) -> String {
    let c = login.choice();
    // The picker names the provider and model in force, so a list that spans every provider is read
    // against something (D88, D89).
    if login.purpose() == Purpose::Model {
        return match login.in_force() {
            Some((provider, model)) if !model.is_empty() => {
                format!(" Model · {provider} / {model} ")
            }
            Some((provider, _)) => format!(" Model · {provider} "),
            None => " Model ".to_owned(),
        };
    }
    if c.custom {
        return " Add an OpenAI-compatible server ".to_owned();
    }
    format!(" Log in to {} ", c.name)
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
                    if let Some(page) = provider::key_page(&login.choice().name) {
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
            let picker = login.purpose() == Purpose::Model;
            let spin = SPINNER[spinner % SPINNER.len()];
            lines.push(Line::from(vec![
                Span::styled(format!("{spin} "), Style::new().fg(theme.warning)),
                Span::raw(if picker {
                    "listing what every provider serves…"
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
            // The ids are padded to one column so the providers line up under each other, and the
            // width comes from the whole list rather than the filtered one, so typing does not make
            // the column jump about (D89).
            let id_width = models
                .iter()
                .map(|p| p.model.width())
                .max()
                .unwrap_or(0)
                .min(inner.saturating_sub(12));
            let top = (selected + 1).saturating_sub(MODEL_ROWS);
            for (i, p) in list.iter().enumerate().skip(top).take(MODEL_ROWS) {
                let chosen = i == *selected;
                lines.push(Line::from(vec![
                    Span::styled(if chosen { "› " } else { "  " }, accent),
                    Span::styled(
                        format!(
                            "{:<width$}",
                            keep_head(&p.model, id_width),
                            width = id_width
                        ),
                        if chosen {
                            Style::new().add_modifier(Modifier::BOLD)
                        } else {
                            Style::new()
                        },
                    ),
                    Span::styled(format!("  {}", p.provider), dim(theme)),
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
            let esc = if login.purpose() == Purpose::Model {
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
            // The two keys do different things, so which is which is said rather than discovered by
            // changing the default by accident (D91).
            if login.purpose() == Purpose::Model {
                lines.push(Line::from(Span::styled(
                    "ctrl+s also makes it the default, for new sessions",
                    dim(theme),
                )));
            }
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
            // A dialog is not the transcript and draws no formula pictures (D166).
            pictures: Vec::new(),
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
fn draw_tree(frame: &mut Frame<'_>, tree: &mut Tree, area: Rect, theme: &Theme) -> Seen {
    // The whole screen, as pi has it: the tree is a mode of the terminal rather than a window in
    // it, so a long conversation is one scroll instead of a scroll inside a scroll (D168).
    let width = area.width as usize;
    let height = area.height as usize;
    let accent = Style::new().fg(theme.accent);
    // The title and the footers are the rows the list does not get, and the list is what the keys
    // are measured in: a page is a screenful, and the scroll keeps the keyboard's row on it. Two
    // key lines rather than one because a narrow terminal was cutting the last of them off.
    let room = height.saturating_sub(5).max(1);
    tree.rows = room;
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            keep_head(
                &if tree.user_only {
                    " conversation tree · prompts only".to_owned()
                } else {
                    " conversation tree".to_owned()
                },
                width,
            ),
            accent.add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    let rows = tree.visible();
    let top = tree.offset();
    let mut drawn = 0;
    for (i, row) in rows.iter().enumerate().skip(top).take(room) {
        let chosen = i == tree.selected;
        let label = keep_head(&tree.label(row), width.saturating_sub(2));
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
        drawn += 1;
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet", dim(theme))));
        drawn += 1;
    }
    // The list fills the screen even where it has nothing to say, so the transcript cannot show
    // through between its rows and the mode looks like a mode.
    for _ in drawn..room {
        lines.push(Line::default());
    }
    let leaves = tree.leaves().len();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let counts = if leaves > 1 {
        format!(
            "{} · {} · {}",
            plural(tree.points.len(), "message"),
            plural(tree.prompts(), "prompt"),
            plural(leaves, "branch end")
        )
    } else {
        format!(
            "{} · {}",
            plural(tree.points.len(), "message"),
            plural(tree.prompts(), "prompt")
        )
    };
    lines.push(Line::from(Span::styled(
        keep_head(&counts, width),
        dim(theme),
    )));
    // The branch ends have a key only if `[keys]` gave them one, so the footer names it when it
    // exists and says nothing about it when it does not (D169).
    let mut move_keys = "↑↓ move".to_owned();
    if leaves > 1
        && let Some(ends) = tree.keys.ends()
    {
        move_keys.push_str(&format!(" · {ends}"));
    }
    move_keys.push_str(" · ctrl+u prompts only");
    for keys in [
        move_keys,
        "enter continue here · ctrl+x copy · esc closes".to_owned(),
    ] {
        lines.push(Line::from(Span::styled(
            keep_head(&keys, width),
            dim(theme),
        )));
    }
    lines.truncate(height);
    // What each row says, cut to the cells it occupies, so a drag copies the tree and not the
    // transcript underneath it (D87).
    let text: Vec<String> = lines
        .iter()
        .map(|line| plain(line).chars().take(width).collect())
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines), area);
    Seen {
        area,
        top: 0,
        lines: text,
        // The tree draws no formula pictures (D166).
        pictures: Vec::new(),
    }
}

fn draw_resume(frame: &mut Frame<'_>, picker: &SessionPicker, area: Rect, theme: &Theme) -> Seen {
    let width = area.width.saturating_sub(4).min(96);
    let inner = width.saturating_sub(2) as usize;
    let accent = Style::new().fg(theme.accent);
    let now = SystemTime::now();
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
fn draw_themes(frame: &mut Frame<'_>, picker: &ThemePicker, area: Rect, theme: &Theme) -> Seen {
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
        lines.push(Line::from(vec![
            Span::styled("add  › ", accent),
            Span::styled(
                typed.clone(),
                Style::new().add_modifier(Modifier::UNDERLINED),
            ),
        ]));
        cursor = Some((lines.len() - 1, 2 + 5 + typed.width()));
        // The entry the line would write, or what is wrong with it, is on screen as it is typed
        // (D173), so nothing is written to find out.
        match &dialog.draft {
            Draft::Ready(preview) => {
                lines.push(Line::from(Span::styled(
                    keep_head(&preview.line, inner),
                    accent,
                )));
                lines.push(Line::from(Span::styled(
                    keep_head(&preview.summary, inner),
                    dim(theme),
                )));
                if let Some(note) = &preview.note {
                    lines.push(Line::from(Span::styled(keep_head(note, inner), dim(theme))));
                }
            }
            Draft::Invalid(e) => lines.push(Line::from(Span::styled(
                keep_head(e, inner),
                Style::new().fg(theme.error),
            ))),
            Draft::Empty => {
                for line in ADD_HELP {
                    lines.push(Line::from(Span::styled(keep_tail(line, inner), dim(theme))));
                }
            }
        }
        if let Some(hint) = dialog.hint() {
            lines.push(Line::from(Span::styled(
                keep_head(&hint, inner),
                dim(theme),
            )));
        }
        // A write can still fail for a reason the line cannot show, such as a file that changed
        // underneath the prompt.
        if let Some(e) = &dialog.error {
            lines.push(Line::from(Span::styled(
                keep_tail(e, inner),
                Style::new().fg(theme.error),
            )));
        }
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
            keep_tail(&dialog.source.policy_path, inner / 2),
            keep_tail(&dialog.source.global_path, inner / 2)
        ),
        dim(theme),
    )));
    let keys = if dialog.adding.is_some() {
        "enter add · tab complete · esc cancel"
    } else if dialog.editing.is_some() {
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
            out.push(mem::take(&mut line));
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

/// Word-wraps plain text to `width` columns and at most `max` lines, marking a cut with `…`.
fn wrap_columns(text: &str, width: usize, max: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if !line.is_empty() && line.width() + 1 + word.width() > width {
                out.push(mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    if out.len() > max {
        out.truncate(max);
        if let Some(last) = out.last_mut() {
            last.push('…');
        }
    }
    out
}

/// The start of `s` in at most `max` columns, with `…` marking the cut.
fn clip(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
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
    iter::once('…').chain(out.into_iter().rev()).collect()
}

/// The whole team on one page: the sessions in a list on the left, and the selected one drawn
/// large on the right with what it has streamed, so moving down the list is the only navigation
/// there is (D195).
fn draw_overview(frame: &mut Frame<'_>, app: &mut App, selected: usize, area: Rect) -> Seen {
    let sessions = app.team.len() + 1;
    let rows = sessions + 2;
    let selected = selected.min(rows - 1);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(app.theme.border_accent))
        .title(" agent sessions ")
        .title_bottom(Line::from(Span::styled(
            "tab/↑↓ select · enter open · PgUp/PgDn history · i/x · esc back",
            dim(&app.theme),
        )));
    frame.render_widget(Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A third of the page for the list, so the detail keeps room for a wrapped paragraph even
    // on a narrow terminal.
    let list_width = (inner.width / 3)
        .clamp(20, 36)
        .min(inner.width.saturating_sub(16));
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)]).areas(inner);
    draw_session_list(frame, app, selected, list_area);
    let (heading, empty) = match selected {
        0 => (
            Line::from(Span::styled(
                " lead ",
                Style::new()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            "your conversation",
        ),
        index if index < sessions => {
            let worker = &app.team[index - 1];
            let state = if worker.state.is_empty() {
                "starting"
            } else {
                &worker.state
            };
            let (glyph, colour) = overview_mark(state, &app.theme);
            (
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", worker.name),
                        Style::new()
                            .fg(app.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{glyph} {state} "), Style::new().fg(colour)),
                    // Which model it is on, so the page says what a worker costs before a person
                    // opens it (D195).
                    Span::styled(format!("· {} ", worker.model), dim(&app.theme)),
                ]),
                "nothing yet",
            )
        }
        _ => {
            let bus = selected == sessions;
            (
                Line::from(Span::styled(
                    if bus { " bus " } else { " command queue " },
                    Style::new()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                if bus {
                    "no messages yet; workers reach each other directly"
                } else {
                    "the queue is empty"
                },
            )
        }
    };
    let detail = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(app.theme.border_accent))
        .title(heading);
    let detail_inner = detail.inner(detail_area);
    frame.render_widget(detail, detail_area);
    match selected {
        // A session's detail is its own transcript, drawn by the lead's renderer from the start
        // of what is kept, so PgUp walks back through its turns and reasoning (D195).
        0 => draw_transcript(frame, app, detail_inner, false),
        index if index < sessions => {
            let worker = app.team[index - 1].clone();
            draw_worker_transcript(frame, &worker, app, detail_inner);
        }
        index => {
            let bus = index == sessions;
            let lines = if bus {
                bus_lines(app)
            } else {
                queue_lines(app)
            };
            draw_board(frame, app, lines, empty, detail_inner);
        }
    }
    // The page is a surface of its own: what was drawn into it is what a drag copies, and the
    // selection's rows map to the cells it covered (D87).
    let lines = (0..inner.height)
        .map(|y| {
            (0..inner.width)
                .map(|x| {
                    frame.buffer_mut()[(inner.x + x, inner.y + y)]
                        .symbol()
                        .to_owned()
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    Seen {
        area: inner,
        top: 0,
        lines,
        pictures: Vec::new(),
    }
}

/// A board is one wrapped paragraph scrolled as a whole: the full messages on the bus, the full
/// commands and reasons in the queue, newest at the end unless the person scrolled back (D196).
fn draw_board(
    frame: &mut Frame<'_>,
    app: &mut App,
    lines: Vec<Line<'static>>,
    empty: &str,
    inner: Rect,
) {
    app.board_height = inner.height as usize;
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(empty, dim(&app.theme)))),
            inner,
        );
        return;
    }
    let wrapped = wrap_styled(lines, inner.width as usize);
    let height = inner.height as usize;
    let start = wrapped
        .len()
        .saturating_sub(height.saturating_add(app.board_scroll.min(wrapped.len())));
    frame.render_widget(Paragraph::new(wrapped[start..].to_vec()), inner);
}

/// Wraps styled lines at a column count, splitting a span and carrying its style onto the next
/// row, so a board's full text can be read and scrolled by row rather than cut to one line
/// (D196).
fn wrap_styled(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for span in line.spans {
            let style = span.style;
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let cells = ch.width().unwrap_or(0);
                if used > 0 && used + cells > width {
                    if !chunk.is_empty() {
                        spans.push(Span::styled(mem::take(&mut chunk), style));
                    }
                    out.push(Line::from(mem::take(&mut spans)));
                    used = 0;
                }
                chunk.push(ch);
                used += cells;
            }
            if !chunk.is_empty() {
                spans.push(Span::styled(chunk, style));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

/// One row per session, the person's own first, then the two team boards — the message bus and
/// the shared command queue — so everything the team shares is in the same list (D195).
fn draw_session_list(frame: &mut Frame<'_>, app: &App, selected: usize, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(app.theme.border_muted))
        .title(" sessions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let name_width = inner.width.saturating_sub(12) as usize;
    let mut rows: Vec<Line> = Vec::new();
    for index in 0..=app.team.len() {
        let chosen = index == selected;
        let (name, state, glyph, colour) = if index == 0 {
            ("lead".to_owned(), "you".to_owned(), '●', app.theme.muted)
        } else {
            let worker = &app.team[index - 1];
            let state = if worker.state.is_empty() {
                "starting"
            } else {
                worker.state.as_str()
            };
            let (glyph, colour) = overview_mark(state, &app.theme);
            (worker.name.clone(), state.to_owned(), glyph, colour)
        };
        let short = state.split_whitespace().next().unwrap_or(state.as_str());
        let mut spans = vec![
            Span::styled(
                if chosen { "▸ " } else { "  " },
                Style::new().fg(app.theme.accent),
            ),
            Span::styled(
                format!("{name:<name_width$}"),
                if chosen {
                    Style::new()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(app.theme.text)
                },
            ),
            Span::styled(format!(" {glyph} "), Style::new().fg(colour)),
        ];
        if chosen {
            spans.push(Span::styled(short.to_owned(), Style::new().fg(colour)));
        }
        rows.push(Line::from(spans));
    }
    // The boards are separate from the conversations: a blank row, then one row each with how
    // much each holds (D195).
    rows.push(Line::default());
    let bus = index_row(
        app.team.len() + 1,
        selected,
        "bus",
        app.bus.len(),
        &app.theme,
    );
    rows.push(bus);
    let waiting = app
        .jobs
        .iter()
        .filter(|item| matches!(item.state, QueueState::Pending | QueueState::Claimed { .. }))
        .count();
    let queue = index_row(app.team.len() + 2, selected, "queue", waiting, &app.theme);
    rows.push(queue);
    frame.render_widget(Paragraph::new(rows), inner);
}

/// One board's row in the session list: its name and how many things it holds (D195).
fn index_row(
    index: usize,
    selected: usize,
    name: &str,
    count: usize,
    theme: &Theme,
) -> Line<'static> {
    let chosen = index == selected;
    Line::from(vec![
        Span::styled(
            if chosen { "▸ " } else { "  " },
            Style::new().fg(theme.accent),
        ),
        Span::styled(
            format!("{name} "),
            if chosen {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text)
            },
        ),
        Span::styled(format!("({count})"), dim(theme)),
    ])
}

/// The live message bus, as a paragraph: who wrote to whom and every line of what was said, so a
/// long finding is readable in full rather than reduced to one row (D196).
fn bus_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for message in &app.bus {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            format!("{} → {}", message.from, message.to),
            Style::new()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for text in message.text.lines() {
            lines.push(Line::from(Span::styled(
                text.to_owned(),
                Style::new().fg(app.theme.text),
            )));
        }
    }
    lines
}

/// The shared command queue as a paragraph: each item's full command and reason, with its state
/// and who enqueued it, so nothing is cut to fit a row (D196).
fn queue_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &app.jobs {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let (state, colour) = match &item.state {
            QueueState::Pending => ("pending".to_owned(), app.theme.muted),
            QueueState::Claimed { by } => (format!("running ({by})"), app.theme.success),
            QueueState::Done { ok: true } => ("done".to_owned(), app.theme.success),
            QueueState::Done { ok: false } => ("failed".to_owned(), app.theme.error),
            QueueState::Lost => ("lost".to_owned(), app.theme.warning),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{} ", item.id),
                Style::new()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(state, Style::new().fg(colour)),
            Span::styled(format!("  · enqueued by {}", item.owner), dim(&app.theme)),
        ]));
        for command in item.command.lines() {
            lines.push(Line::from(Span::styled(
                command.to_owned(),
                Style::new().fg(app.theme.text),
            )));
        }
        if !item.why.is_empty() {
            for why in item.why.lines() {
                lines.push(Line::from(Span::styled(
                    format!("why: {why}"),
                    dim(&app.theme),
                )));
            }
        }
    }
    lines
}

/// What a worker is doing, as one glyph and the colour it is drawn in (D195).
fn overview_mark(state: &str, theme: &Theme) -> (char, Color) {
    if state.starts_with("running") {
        ('●', theme.success)
    } else if state.starts_with("finished") {
        ('✓', theme.muted)
    } else if state.starts_with("failed") || state.starts_with("stopped") {
        ('✗', theme.error)
    } else if state.starts_with("idle") {
        ('○', theme.muted)
    } else {
        ('…', theme.dim)
    }
}

/// A worker's own conversation, drawn by the lead's transcript renderer — the same markdown,
/// highlighted code, tool blocks, collapsed output, scrolling and pictures — under one line that
/// says whose transcript this is (D195).
fn draw_worker(frame: &mut Frame<'_>, worker: &Render, app: &mut App, area: Rect) {
    let state = if worker.state.is_empty() {
        "starting"
    } else {
        worker.state.as_str()
    };
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "▸ agent transcript ",
                Style::new()
                    .fg(app.theme.border_accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} · {state}", worker.name),
                Style::new().fg(app.theme.accent),
            ),
            Span::styled(
                "  · esc/ctrl+b sessions · ↑↓ scroll · enter steer",
                dim(&app.theme),
            ),
        ])),
        head,
    );
    draw_worker_transcript(frame, worker, app, body);
}

/// The body of a worker view or of its tab in the overview: the lead's own transcript path, with
/// the worker's conversation swapped in. The rendering, the collapse, the scroll arithmetic and
/// the picture placement are one implementation, so the views cannot drift apart (D195).
fn draw_worker_transcript(frame: &mut Frame<'_>, worker: &Render, app: &mut App, body: Rect) {
    let entries = worker_entries(worker);
    let fresh = iter::repeat_with(|| None).take(entries.len()).collect();
    let lead_cache = mem::replace(&mut app.cache, fresh);
    let lead_entries = mem::replace(&mut app.entries, entries);
    let (lead_scroll, lead_total) = (app.scroll, app.total_lines);
    app.scroll = app.worker_scroll;
    draw_transcript(frame, app, body, false);
    app.worker_scroll = app.scroll;
    app.worker_total = app.total_lines;
    app.worker_height = app.view_height;
    app.scroll = lead_scroll;
    app.total_lines = lead_total;
    app.entries = lead_entries;
    app.cache = lead_cache;
}

/// A worker's beats as the transcript entries the lead's renderer takes: streamed text and
/// reasoning join the block before them, and a result belongs to the call it answered (D195).
fn worker_entries(worker: &Render) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for beat in &worker.beats {
        match beat {
            Beat::Prompt(text) => out.push(Entry::User(text.clone())),
            Beat::Text(text) => match out.last_mut() {
                Some(Entry::Assistant(held)) => held.push_str(text),
                _ => out.push(Entry::Assistant(text.clone())),
            },
            Beat::Thinking(text) => match out.last_mut() {
                Some(Entry::Thinking(held)) => held.push_str(text),
                _ => out.push(Entry::Thinking(text.clone())),
            },
            Beat::Call {
                name,
                detail,
                timeout,
            } => out.push(Entry::Tool {
                id: String::new(),
                tasks: Vec::new(),
                name: name.clone(),
                detail: detail.clone(),
                result: None,
                diff: None,
                timeout: *timeout,
                spent: None,
            }),
            Beat::Result {
                ok,
                content,
                diff,
                images,
            } => {
                // A picture a tool returned is kept the way an attached one is, above the block
                // that answered (D58, D59).
                for image in images {
                    out.push(Entry::Image(image.clone()));
                }
                match out
                    .iter_mut()
                    .rev()
                    .find(|entry| matches!(entry, Entry::Tool { result: None, .. }))
                {
                    Some(Entry::Tool {
                        result, diff: held, ..
                    }) => {
                        *result = Some((*ok, content.clone()));
                        *held = diff.clone();
                    }
                    _ => out.push(Entry::Note(NoteKind::Info, content.clone())),
                }
            }
            Beat::Note(text) => out.push(Entry::Note(NoteKind::Info, text.clone())),
            Beat::Spend { input, output, .. } => out.push(Entry::Note(
                NoteKind::Info,
                format!("{input} in · {output} out"),
            )),
        }
    }
    out
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect, activity: bool) {
    let (mut lines, at) = transcript_with_images(app, area.width.max(1), activity);
    // A question a tool is waiting on is the end of the transcript, not a lid over it (D197): the
    // rows go after everything said, the transcript's own scrolling reaches them, and a person
    // reading what led to the question scrolls exactly as ever.
    let question = app.ask.as_ref().map(|dialog| {
        ask_rows(
            dialog,
            area.width as usize,
            Some(area.height as usize),
            &app.theme,
        )
    });
    let cursor = question.as_ref().and_then(|(_, cursor)| *cursor);
    let asked = question.as_ref().map_or(0, |(rows, _)| rows.len());
    if let Some((rows, _)) = question {
        lines.extend(rows);
    }
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
        || app.confirm.is_some()
        || app.settings.is_some()
        || app.permissions.is_some()
        || app.themes.is_some()
        || app.login.is_some()
        || app.overview.is_some();
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
    // rows is: the marker scrolling off the top does not take the picture with it (D59). Its cells
    // and its pixels come from the layout, which is what stops a picture from being drawn at a size
    // other than the one the rows were reserved for (D165).
    // Where each formula's words belong, so a drag over the picture copies the TeX it was drawn
    // from rather than the blank cells it left on screen (D166). Built for every picture, not only
    // the ones on screen: a selection can start above them.
    let sources: Vec<Source> = at
        .iter()
        .filter(|at| !at.tex.is_empty())
        .map(|at| Source {
            line: at.line,
            // A block formula starts its row, and its box is the rows the layout gave it.
            column: at.column.unwrap_or(0),
            rows: at.picture.cells.rows.max(1),
            cols: at.picture.cells.cols.max(1),
            inline: at.inline,
            typst: at.typst,
            tex: at.tex.clone(),
        })
        .collect();
    for At {
        line,
        column,
        picture,
        id,
        ..
    } in at
    {
        let Picture { image, cells: box_ } = picture;
        if image::band(line, box_.rows, top, height).is_none() {
            continue;
        }
        placed.push(Placed {
            line,
            column: column.unwrap_or(0),
            id,
            box_,
            pixels: (image.width, image.height),
            data: image.data,
        });
    }
    let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
    // The cursor of the line being written sits in the block, unless the person has scrolled it
    // off the screen, in which case nothing on this screen has the cursor (D197).
    if let Some((row, col)) = cursor {
        let line = total.saturating_sub(asked) + row;
        if line >= top && line < top + height {
            frame.set_cursor_position(Position::new(
                area.x + (col as u16).min(area.width.saturating_sub(1)),
                area.y + (line - top) as u16,
            ));
        }
    }
    app.images = placed;
    app.seen = Some(Seen {
        area,
        top,
        lines: text,
        pictures: sources,
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
    use std::borrow::Cow;

    use super::*;
    use crate::agent::End;
    use crate::provider::Image;
    use crate::provider::Usage;
    use crate::questions::Asked;
    use crate::store::{Point, PointKind, Summary};
    use crate::team::{Beat, Render};
    use crate::tui::app::{Action, Cell, Selection};
    use crate::tui::app::{AgentEvent, Header, Spent};
    use crate::tui::keys::Keys;
    use crate::tui::notify::Notify;
    use crate::tui::permissions::{
        Kind, Permissions, Row as PermissionRow, Source as PermissionSource,
    };
    use crate::tui::settings::Values;
    use crate::tui::status;
    use crate::tui::status::Template;
    use crate::tui::theme;
    use crate::tui::theme::{Theme, ThemeTables};
    use crate::tui::tree::Tree;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use saddle_wit::saddle::host::ask::{Choice as AskChoice, Question as AskedQuestion};
    use std::time::Duration;
    use std::time::SystemTime;

    fn img(name: &str, w: u32, h: u32) -> Image {
        Image {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".into(),
            name: name.into(),
            width: w,
            height: h,
        }
    }

    #[test]
    fn plugin_panels_draw_in_labelled_frames_and_clear_without_transcript_entries() {
        let mut app = bare_app();
        let entries = app.entries.len();
        app.agent(AgentEvent::Panels(vec![PanelView {
            plugin: "plugin.features".into(),
            title: "background work".into(),
            rows: vec![PanelRow::Progress {
                completed: 1,
                total: 2,
                label: "index".into(),
            }],
        }]));
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("plugin.features · background work"));
        assert!(rendered.contains("index ["));
        assert_eq!(app.entries.len(), entries);
        app.agent(AgentEvent::Panels(Vec::new()));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("plugin.features"));
    }

    #[test]
    fn two_attached_pictures_are_both_placed() {
        // Two images in one conversation is the ordinary case for "@a.png @b.png" (D59).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
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
        let area = Rect::new(0, 0, 80, 40);
        let plan = plan_images(
            image::Protocol::Kitty,
            &app.drawn,
            &app.images,
            area,
            0,
            &[],
        );
        assert_eq!(plan.place.len(), 2, "{plan:?}");
        assert_ne!(
            plan.place[0].0, plan.place[1].0,
            "on different rows: {plan:?}"
        );
    }

    #[test]
    fn the_rows_left_for_a_picture_are_the_box_it_is_drawn_in() {
        // One fit, one width: the layout reserves blank rows from the same computation the drawing
        // pass puts the picture in. They were worked out on widths four columns apart, so a picture
        // that fitted the wider room left a row spare and, at the boundary, had the next row of the
        // transcript drawn over its last one (D59). 1000×400 is scaled here, which is the case
        // where the two widths disagree: the wider room gives it a row more than the narrower one.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::User("look".into()));
        app.cache.push(None);
        app.entries.push(Entry::Image(img("shot.png", 1000, 400)));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let area = app.seen.as_ref().unwrap().area;
        let box_ = image::fit((1000, 400), (area.width, image::MAX_ROWS));
        assert_eq!(app.images[0].box_, box_, "fitted to the transcript width");
        // The marker line, then one blank row per row of the picture, and nothing else: this entry
        // is the last one in the transcript. Four columns narrower and the fit is one row shorter,
        // which is what the layout used to reserve against.
        let (lines, at) = transcript_with_images(&mut app, area.width, false);
        let start = at[0].line;
        let reserved = lines.len().saturating_sub(start);
        assert_eq!(reserved, box_.rows as usize, "{box_:?}");
        assert_ne!(
            image::fit((1000, 400), (area.width - 4, image::MAX_ROWS)).rows,
            box_.rows,
            "this picture is the case where the two widths disagree"
        );
    }

    #[test]
    fn a_picture_is_placed_while_any_of_it_is_on_screen() {
        // The bug: placement was skipped unless the picture's *marker line* was inside the view, so
        // scrolling the marker off the top made the picture disappear although nearly all of it was
        // still on screen. The marker is a caption, not the picture (D59).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
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
        let (_, at) = transcript_with_images(&mut app, width, false);
        let start = at[0].line;

        // Put the picture's own first row above the top of the view, so that one row of it is off
        // screen and the rest is visible.
        let want_top = start + 1;
        app.scroll = total.saturating_sub(height + want_top);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.as_ref().unwrap();
        assert_eq!(
            seen.top, want_top,
            "the picture's first row is off the top of the screen"
        );

        assert_eq!(
            app.images.len(),
            1,
            "the picture is still placed: {:?}",
            app.images
        );
        let placed = &app.images[0];
        assert_eq!(placed.line, start);
        // And the drawing pass crops it to the rows that are on screen, at the top of the area.
        let plan = plan_images(
            image::Protocol::Kitty,
            &app.drawn,
            &app.images,
            seen.area,
            seen.top,
            &[],
        );
        assert_eq!(plan.place.len(), 1, "{plan:?}");
        assert_eq!(
            plan.place[0].0, 0,
            "drawn from the top of the area: {plan:?}"
        );
        assert_eq!(
            plan.drawn.shown[0].3, 1,
            "one row of the picture is above the fold: {:?}",
            plan.drawn.shown
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
            activity: false,
            room: (80, 24),
            images: false,
            scale: 1.0,
        };
        entry_rows(&Entry::User(text.into()), view, &theme, &mut Vec::new())
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
                activity: false,
                room: (80, 24),
                images: false,
                scale: 1.0,
            },
            &Theme::default(),
            &mut Vec::new(),
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
                activity: false,
                room: (80, 24),
                images: false,
                scale: 1.0,
            },
            &themed,
            &mut Vec::new(),
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
                activity: false,
                room: (80, 24),
                images: false,
                scale: 1.0,
            },
            &theme,
            &mut Vec::new(),
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
        let row = Row {
            first: vec![Span::raw("› ")],
            rest: vec![Span::raw("  ")],
            spans: vec![Span::raw("aaa bbb ccc")],
        };
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
    fn a_finished_compaction_shows_its_first_lines_and_ctrl_o_shows_the_rest() {
        // D124: the summary is collapsed like a tool's output, so a long one does not push the
        // conversation up by thirty lines every time the context is compacted.
        let summary = (1..=20)
            .map(|n| format!("line {n} of the summary"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = |state: CompactState, expand: bool| {
            let view = CompactionView {
                header: "24 messages".to_owned(),
                reasoning: String::new(),
                summary: summary.clone(),
                state,
                blocks: true,
            };
            let (rows, _) = compaction_block(&view, false, expand, 0, &Theme::default(), 80);
            rows.iter()
                .flat_map(|r| text(&wrap(r, 80)))
                .collect::<Vec<_>>()
        };

        let collapsed = block(CompactState::Done(String::new()), false);
        assert!(
            collapsed
                .iter()
                .any(|l| l.contains("line 1 of the summary")),
            "the gist is not shown: {collapsed:#?}"
        );
        assert!(
            collapsed
                .iter()
                .any(|l| l.contains("… 15 more lines · ctrl+o shows all")),
            "a collapsed summary does not say how much it hides: {collapsed:#?}"
        );
        assert!(
            !collapsed
                .iter()
                .any(|l| l.contains("line 20 of the summary")),
            "a collapsed summary showed its end: {collapsed:#?}"
        );
        assert!(
            collapsed.len() < 12,
            "a collapsed summary took {} lines",
            collapsed.len()
        );

        let expanded = block(CompactState::Done(String::new()), true);
        assert!(
            expanded
                .iter()
                .any(|l| l.contains("line 20 of the summary")),
            "ctrl+o did not show the whole summary: {expanded:#?}"
        );

        // While it streams, the lines that just arrived are the informative end, as for bash.
        let running = block(CompactState::Running, false);
        assert!(
            running.iter().any(|l| l.contains("line 20 of the summary")),
            "a streaming summary hides the text that just arrived: {running:#?}"
        );
        assert!(
            running
                .iter()
                .any(|l| l.contains("… 15 earlier lines · ctrl+o shows all")),
            "a streaming summary does not say how much came before: {running:#?}"
        );
    }

    #[test]
    fn a_compaction_collapses_its_reasoning_with_the_summary() {
        // D124: the entry is one thing, opened by one key. The reasoning a compaction was written
        // from used to follow ctrl+t, so a session with reasoning on showed every compaction with
        // all of its thinking laid out — the opposite of collapsing it.
        let reasoning = (1..=12)
            .map(|n| format!("thought {n} of the compaction"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = |thinking: bool, expand: bool| {
            let view = CompactionView {
                header: "24 messages".to_owned(),
                reasoning: reasoning.clone(),
                summary: "kept: the parser rewrite".to_owned(),
                state: CompactState::Done(String::new()),
                blocks: true,
            };
            let (rows, _) = compaction_block(&view, thinking, expand, 0, &Theme::default(), 80);
            rows.iter()
                .flat_map(|r| text(&wrap(r, 80)))
                .collect::<Vec<_>>()
        };

        // The default: reasoning on, ctrl+o never pressed. One line that says what is there.
        let collapsed = block(true, false);
        assert!(
            collapsed
                .iter()
                .any(|l| l.contains("∴ reasoned for 12 lines · ctrl+o shows all")),
            "a collapsed compaction does not offer the key that opens it: {collapsed:#?}"
        );
        assert!(
            !collapsed.iter().any(|l| l.contains("thought 1 of")),
            "the reasoning was laid out unasked: {collapsed:#?}"
        );

        // ctrl+o opens the entry, and the reasoning is part of what it opens.
        let expanded = block(true, true);
        assert!(
            expanded.iter().any(|l| l.contains("thought 1 of")),
            "ctrl+o did not open the reasoning with the rest: {expanded:#?}"
        );

        // A person who turned reasoning off keeps it off, and the note says which key would show
        // it rather than promising one that would not.
        let off = block(false, true);
        assert!(!off.iter().any(|l| l.contains("thought 1 of")), "{off:#?}");
        assert!(
            off.iter().any(|l| l.contains("· ctrl+t shows it")),
            "{off:#?}"
        );
    }

    /// Render options for a block drawn by a test: a width, and whether it is expanded.
    fn view(expand: bool) -> View {
        View {
            expand,
            thinking: false,
            spinner: 0,
            activity: false,
            room: (80, image::MAX_ROWS),
            images: false,
            scale: 1.0,
        }
    }

    #[test]
    fn tool_blocks_show_the_call_and_the_useful_end_of_the_output() {
        let block = |name: &str, detail: &str, ok: bool, content: &str, expand: bool| {
            let (rows, bg) = tool_block(
                name,
                detail,
                Some(&(ok, content.into())),
                None,
                &view(expand),
                Timing {
                    elapsed: None,
                    limit: None,
                },
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
        // A call with nothing to name falls back to the tool's own name: a lone `$` with a
        // stopwatch beside it is what a polled task looked like (D181).
        let (lines, _) = block("bash", "", true, "exit code 0", false);
        assert_eq!(lines.first().map(String::as_str), Some("bash"), "{lines:?}");
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

        let (rows, bg) = tool_block(
            "bash",
            "sleep 9",
            None,
            None,
            &view(false),
            Timing {
                elapsed: None,
                limit: None,
            },
            &Theme::default(),
        );
        let diff = "~\n-12 old line\n+12 new line, long enough to wrap around\n 13 next\n";
        let (edit_rows, _) = tool_block(
            "edit",
            "src/x.rs",
            Some(&(true, "edited src/x.rs at line 12".into())),
            Some(diff),
            &view(false),
            Timing {
                elapsed: None,
                limit: None,
            },
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
    fn a_call_in_flight_spins_at_the_bottom_and_not_in_its_block() {
        // D172: the frame's activity line already says what is running, so the block it is running
        // from does not lead with a second spinner. The pending shade and the stopwatch still mark
        // the call, and a worker's view keeps its block spinner because that frame has no activity
        // line of its own.
        let mut app = bare_app();
        let now = Instant::now();
        app.now = now;
        app.agent(AgentEvent::Call {
            id: "c1".into(),
            name: "bash".into(),
            detail: "sleep 9".into(),
            timeout: None,
        });
        if let Some(Entry::Tool { spent, .. }) = app.entries.last_mut() {
            *spent = Some(Spent::Running(now - Duration::from_secs(5)));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("running bash"), "{shown}");
        assert!(shown.contains("$ sleep 9 · 5.0s"), "{shown}");
        assert_eq!(shown.matches('⠋').count(), 1, "one spinner: {shown}");

        let mut worker = Render {
            name: "runtime".into(),
            state: "running".into(),
            ..Render::default()
        };
        worker.beats = vec![Beat::Call {
            name: "bash".into(),
            detail: "sleep 9".into(),
            timeout: None,
        }];
        app.agent(AgentEvent::Workers(vec![worker]));
        app.worker = Some("runtime".into());
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let viewed = screen(&terminal);
        assert!(viewed.contains("⠋ $ sleep 9"), "{viewed}");
    }

    #[test]
    fn a_call_with_a_deadline_says_what_it_is_capped_at() {
        // D152: the deadline the model set belongs on the call's own line — after the stopwatch
        // while it runs, and as a bare limit where there is no stopwatch to put it after.
        let (rows, _) = tool_block(
            "bash",
            "sleep 9",
            None,
            None,
            &view(false),
            Timing {
                elapsed: Some("5.0s"),
                limit: Some(30),
            },
            &Theme::default(),
        );
        assert_eq!(text(&wrap(&rows[0], 80)), ["⠋ $ sleep 9 · 5.0s of 30s"]);
        let (rows, _) = tool_block(
            "read",
            "src/lib.rs",
            None,
            None,
            &view(false),
            Timing {
                elapsed: None,
                limit: Some(30),
            },
            &Theme::default(),
        );
        assert_eq!(text(&wrap(&rows[0], 80)), ["⠋ read src/lib.rs · 30s limit"]);
    }

    #[test]
    fn the_transcript_shows_the_deadline_the_call_was_given() {
        // The whole path: the call event carries the timeout, and the frame draws it beside the
        // stopwatch the frame's own clock keeps.
        let mut app = bare_app();
        let now = Instant::now();
        app.now = now;
        app.agent(AgentEvent::Call {
            id: "c1".into(),
            name: "bash".into(),
            detail: "cargo test".into(),
            timeout: Some(60),
        });
        if let Some(Entry::Tool { spent, .. }) = app.entries.last_mut() {
            *spent = Some(Spent::Running(now - Duration::from_secs(5)));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("$ cargo test · 5.0s of 60s"), "{shown}");
    }

    #[test]
    fn the_transcript_ticks_a_running_command_and_a_running_task_from_its_own_start() {
        // The whole path, not just the block: the frame's clock times a call in flight (D172), and a
        // task is timed from when the session started it, which may be before this interface drew a
        // row for it.
        let mut app = bare_app();
        let now = Instant::now();
        app.now = now;
        app.agent(AgentEvent::Call {
            id: "c1".into(),
            name: "bash".into(),
            detail: "sleep 9".into(),
            timeout: None,
        });
        if let Some(Entry::Tool { spent, .. }) = app.entries.last_mut() {
            *spent = Some(Spent::Running(now - Duration::from_secs(5)));
        }
        let (lines, _) = transcript_with_images(&mut app, 80, false);
        let shown = lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(shown.contains("⠋ $ sleep 9 · 5.0s"), "{shown}");

        // The launch answered, and the work it started is still going with its own timer.
        app.agent(AgentEvent::Result {
            ok: true,
            content: "started task-1".into(),
            diff: None,
            images: Vec::new(),
        });
        app.agent(AgentEvent::Tasks(vec![TaskView {
            id: "task-1".into(),
            call: Some("c1".into()),
            state: TaskState::Running,
            started: now - Duration::from_secs(12),
            stopped: None,
        }]));
        app.now = now;
        let (lines, _) = transcript_with_images(&mut app, 120, false);
        let shown = lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(shown.contains("[task-1: running · 12s]"), "{shown}");
        assert!(shown.contains("⠋ $ sleep 9"), "still pending: {shown}");

        // Once it lands, the task keeps the time it ran rather than counting on.
        app.agent(AgentEvent::Tasks(vec![TaskView {
            id: "task-1".into(),
            call: Some("c1".into()),
            state: TaskState::Succeeded,
            started: now - Duration::from_secs(12),
            stopped: Some(now - Duration::from_secs(2)),
        }]));
        app.now = now + Duration::from_secs(30);
        let (lines, _) = transcript_with_images(&mut app, 120, false);
        let shown = lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(shown.contains("[task-1: completed · 10s]"), "{shown}");
    }

    #[test]
    fn the_status_line_counts_only_running_background_tasks() {
        let mut app = bare_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let now = Instant::now();
        let task = |id: &str, state| TaskView {
            id: id.into(),
            call: None,
            state,
            started: now,
            stopped: (state != TaskState::Running).then_some(now),
        };
        app.agent(AgentEvent::Tasks(vec![
            task("a", TaskState::Running),
            task("b", TaskState::Running),
            task("c", TaskState::Succeeded),
        ]));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains(" · 2 tasks · "));

        app.agent(AgentEvent::Tasks(vec![
            task("a", TaskState::Running),
            task("b", TaskState::Cancelled),
        ]));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains(" · 1 task · "));

        app.agent(AgentEvent::Tasks(vec![task("a", TaskState::Failed)]));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(!shown.contains("task"), "{shown}");
        assert!(!shown.contains(" ·  · "), "{shown}");
    }

    #[test]
    fn a_call_in_flight_and_a_finished_one_both_show_how_long_it_took() {
        // D172: the stopwatch sits after the call, on the header row, and the spinner still leads it
        // while it runs.
        let (rows, bg) = tool_block(
            "bash",
            "sleep 9",
            None,
            None,
            &view(false),
            Timing {
                elapsed: Some("5.0s"),
                limit: None,
            },
            &Theme::default(),
        );
        assert_eq!(text(&wrap(&rows[0], 80)), ["⠋ $ sleep 9 · 5.0s"]);
        assert_eq!(bg, Theme::default().tool_pending_bg);
        let (rows, bg) = tool_block(
            "bash",
            "cargo test",
            Some(&(true, "exit code 0".into())),
            None,
            &view(false),
            Timing {
                elapsed: Some("1m 05s"),
                limit: None,
            },
            &Theme::default(),
        );
        assert_eq!(text(&wrap(&rows[0], 80)), ["$ cargo test · 1m 05s"]);
        assert_eq!(bg, Theme::default().tool_success_bg);
        // A resumed call has no timing of its own, so it shows none.
        let (rows, _) = tool_block(
            "bash",
            "cargo test",
            Some(&(true, "exit code 0".into())),
            None,
            &view(false),
            Timing {
                elapsed: None,
                limit: None,
            },
            &Theme::default(),
        );
        assert_eq!(text(&wrap(&rows[0], 80)), ["$ cargo test"]);
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
        let tables = ThemeTables::new();
        let mut app = bare_app();
        app.theme = theme::build("catppuccin-mocha", &tables).unwrap();
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
        let choice = |name: &str, oauth: bool| Choice {
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
        let mut login = Login::new(
            vec![choice("openai", false), choice("openai-chatgpt", true)],
            vec![],
            Some("openai-chatgpt"),
            true,
            "~/.local/state/saddle/auth.json".into(),
        );
        let Some(Outcome::SignIn(id, _)) = login.take_pending() else {
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
    fn the_question_about_leaving_a_branch_is_drawn_over_the_transcript() {
        // D167: the question is a dialog of its own, so it is drawn over the transcript and owns
        // the selection while it is up, like every other one (D87).
        let mut app = bare_app();
        app.agent(AgentEvent::Text("a reply on the branch".into()));
        app.confirm = Some(Confirm {
            left: 3,
            rewind_to: Some(5),
            text: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("3 messages would be left behind"),
            "the question is not on screen:\n{text}"
        );
        assert!(
            text.contains("y summarize · n do not · esc stay where you are"),
            "the keys are not on screen:\n{text}"
        );
        assert!(
            text.contains("Leave this branch"),
            "the title is not on screen:\n{text}"
        );
        assert!(app.dialog_seen.is_some(), "the question is its own surface");
    }

    #[test]
    fn a_drag_inside_a_dialog_copies_the_dialog_text() {
        // D87: a dialog owns the screen while it is open, so a drag there means its text. Without
        // this the text was drawn and unselectable, which is what the sign-in screen made obvious.
        let mut app = bare_app();
        app.settings = Some(SettingsDialog::new(Values {
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
        }));
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
        let Some(Action::Copy(text, what)) = copied else {
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
        app.settings = Some(SettingsDialog::new(Values {
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
        }));
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
                let inside = seen.area.contains(Position::new(x, y));
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
        let mut app = bare_app();
        app.entries.push(Entry::User("a transcript line".into()));
        app.cache.push(None);
        app.settings = Some(SettingsDialog::new(Values {
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
        }));
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
        app.settings = Some(SettingsDialog::new(Values {
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
        }));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.selection, None, "the old selection did not survive");
    }

    #[test]
    fn the_model_picker_shows_every_providers_list_and_names_the_model_in_force() {
        // D89: the list spans providers, so the screen has to say which one serves each model, and
        // name the model in force so the list is read against something.
        let mut app = bare_app();
        let choice = |name: &str, model: &str| Choice {
            name: name.into(),
            detail: String::new(),
            status: "logged in".into(),
            needs_key: true,
            has_key: true,
            model: Some(model.into()),
            custom: false,
            oauth: false,
        };
        let mut login = Login::for_all_models(
            vec![
                choice("synthetic", "gpt-5.6-sol"),
                choice("openai", "gpt-5.4"),
            ],
            "synthetic",
            "gpt-5.6-sol",
            "auth.json".into(),
        );
        let Some(Outcome::ListAll(id)) = login.take_pending() else {
            panic!("a listing to ask for");
        };
        login.models(
            id,
            Ok(Listed {
                picks: vec![
                    Pick {
                        provider: "synthetic".into(),
                        model: "gpt-5.6-luna".into(),
                    },
                    Pick {
                        provider: "synthetic".into(),
                        model: "gpt-5.6-sol".into(),
                    },
                    Pick {
                        provider: "openai".into(),
                        model: "gpt-5.4".into(),
                    },
                ],
                failed: Vec::new(),
            }),
        );
        app.open_login(login);
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Model · synthetic / gpt-5.6-sol"), "{text}");
        for m in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.4"] {
            assert!(text.contains(m), "{m} is missing:\n{text}");
        }
        // Both providers are named, which is what makes a union list readable.
        for p in ["synthetic", "openai"] {
            assert!(text.contains(p), "{p} is missing:\n{text}");
        }
        assert!(text.contains("type to filter"), "{text}");
        assert!(text.contains("esc close"), "{text}");
        // The model in force is where the highlight starts.
        assert!(
            matches!(
                &app.login.as_ref().unwrap().stage,
                Stage::Models { selected: 1, .. }
            ),
            "the highlight starts on the current model: {:?}",
            app.login.as_ref().unwrap().stage
        );

        // Typing a provider narrows to it; typing a model narrows to that.
        for c in "openai".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("filter  › openai"), "{text}");
        assert!(
            !text.contains("gpt-5.6-luna"),
            "the filter hides the other provider:\n{text}"
        );
        assert!(text.contains("gpt-5.4"), "{text}");
    }

    #[test]
    fn the_sign_in_link_is_a_thing_a_click_can_open() {
        // D86: a drawn link is the terminal's to ignore, and a dialog's text is not selectable, so
        // the address has to be a target. What is checked here is that the cells the click lands on
        // are the cells the address was drawn in.
        let mut app = bare_app();
        let choice = Choice {
            name: "openai-chatgpt".into(),
            detail: String::new(),
            status: "not signed in".into(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        };
        let mut login = Login::new(
            vec![choice],
            vec![],
            Some("openai-chatgpt"),
            true,
            "auth.json".into(),
        );
        let Some(Outcome::SignIn(id, _)) = login.take_pending() else {
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
            Some(Action::OpenUrl(
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
        let Some(Action::Copy(text, _)) = copied else {
            panic!("a drag on the link must select it, got {copied:?}");
        };
        assert!(text.starts_with("http"), "{text}");
    }

    #[test]
    fn a_formula_asks_for_the_rows_it_reserved_rather_than_its_pixel_size() {
        // D165: a formula has no size of its own for the terminal to preserve — the rows the
        // transcript reserved are its size. It therefore carries a cell box, so a terminal whose
        // cell is bigger than the one saddle measured (a display scaled 2x, a query never answered)
        // still draws it across those rows instead of at its own pixel count.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "and therefore $$\\frac{1}{3}$$ holds".into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 1, "one formula, one picture");
        let At { line, picture, .. } = &at[0];
        // The marker line is blanked, and the picture's cells are the box the terminal is asked
        // for — not a size computed from its pixels.
        assert_eq!(plain(&lines[*line]), "");
        let Picture { image, cells } = picture;
        assert!(cells.scaled, "a formula is always drawn in a cell box");
        assert_eq!(cells.rows, 3, "a fraction is three rows tall: {cells:?}");
        assert!(
            image.height > cells.rows as u32 * image::cell_px().1,
            "the raster is finer than the box: {:?}",
            image
        );
    }

    #[test]
    fn a_formula_written_the_other_ways_people_write_it_is_typeset() {
        // D165: the parser knows `$x$` and `$$x$$`. The two spellings a person writing by hand
        // reaches for first — `\(x\)` and a padded `$ x $` — reach it as prose, so they are
        // rewritten before it parses anything.
        assert_eq!(delimiters(r"so \(x^2\) is").as_ref(), "so $x^2$ is");
        assert_eq!(
            delimiters(r"\[ \frac{a}{b} \]").as_ref(),
            "$$\\frac{a}{b}$$"
        );
        assert_eq!(delimiters("a $ x^2 $ b").as_ref(), "a $x^2$ b");
        assert_eq!(delimiters("a $ x $ b").as_ref(), "a $x$ b");
        assert_eq!(delimiters(r"$ \theta $").as_ref(), "$\\theta$");
        // Money is not a formula, and neither is prose between two dollar signs.
        let money = "costs $5 and $10 today";
        assert_eq!(delimiters(money).as_ref(), money);
        let prose = "from $ here to $ there";
        assert_eq!(delimiters(prose).as_ref(), prose);
        // Code is read exactly as it was written, which is what code is for.
        let fenced = "```\n\\(x\\) and $ y $\n```\n";
        assert_eq!(delimiters(fenced).as_ref(), fenced);
        let span = "a `\\(x\\)` b";
        assert_eq!(delimiters(span).as_ref(), span);
        // And the text after a fence is still text: read as an inline span, the closing fence's
        // backticks opened one that never closed, and every formula after the block was left as
        // its source — which is what a real terminal showed (D165).
        let after = "```\nx\n```\n\nso \\(x^2\\) holds, and `a` too.";
        assert_eq!(
            delimiters(after).as_ref(),
            "```\nx\n```\n\nso $x^2$ holds, and `a` too."
        );
        // A backtick with no partner is a backtick, not the end of the message.
        assert_eq!(delimiters("a ` b and \\(x\\)").as_ref(), "a ` b and $x$");
        // Nothing to rewrite means the text is parsed where it is, without a copy.
        assert!(matches!(delimiters("a $x$ b"), Cow::Borrowed(_)));
        assert!(matches!(delimiters("plain prose"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_formula_in_tex_s_own_delimiters_is_drawn_too() {
        // End to end: `\(…\)` is what a mathematician types, and it reaches the same picture as
        // `$…$` does (D165).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries
            .push(Entry::Assistant("so \\(x^2\\) holds".into()));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 1, "one formula, one picture");
        assert_eq!(at[0].column, Some(3), "{:?}", plain(&lines[at[0].line]));
        assert!(
            lines.iter().all(|l| !plain(l).contains("\\(x^2\\)")),
            "the source is not what is shown"
        );
    }

    #[test]
    fn a_copy_of_a_drawn_formula_is_the_tex_it_was_drawn_from() {
        // D166: end to end, through the drawing pass. The layout is what knows where each picture
        // stands, so the copy is tested from there: a drag over the whole transcript takes the words
        // each formula was typeset from, in places where the screen has blank cells.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "by parts\n\n$$\\int u\\,dv=uv-\\int v\\,du$$\n\nso \\(x^2\\) holds".into(),
        ));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.clone().expect("the transcript was drawn");
        assert_eq!(
            seen.pictures.len(),
            2,
            "one block and one in a line: {:?}",
            seen.pictures
        );
        let last = seen.lines.len() - 1;
        app.selection = Some(Selection {
            anchor: Cell { line: 0, ch: 0 },
            focus: Cell { line: last, ch: 60 },
            dragging: false,
        });
        let text = app.selected_text().expect("a copy");
        assert!(
            text.contains("\\[\\int u\\,dv=uv-\\int v\\,du\\]"),
            "{text:?}"
        );
        assert!(text.contains("\\(x^2\\)"), "{text:?}");
        // What was drawn over is not in the copy twice, and the source text the picture replaced is
        // not there either.
        assert!(!text.contains("$$"), "{text:?}");
        assert_eq!(text.matches("x^2").count(), 1, "{text:?}");
    }

    #[test]
    fn a_fence_that_says_typst_math_is_a_formula_and_a_plain_typst_fence_is_code() {
        // D165: the same body cannot be both. `$…$` and `$$…$$` are LaTeX and only LaTeX, so the
        // notation that would otherwise be read as six letters there has an invocation of its own —
        // and a fence tagged `typst`, which is how Typst code is shown, stays code.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "the rule\n\n```typst-math\nfrac(a, b)\n```\n\nand\n\n```typst\nfrac(a, b)\n```\n"
                .into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 1, "one fence is a formula");
        let shown: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(
            shown.iter().filter(|l| l.contains("frac(a, b)")).count(),
            1,
            "the code block still shows the text it was written with: {shown:?}"
        );
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with("  ") && l.contains("frac(a, b)")),
            "it is drawn as a code block, in the block's own colour and indent: {shown:?}"
        );
        assert!(
            shown.iter().all(|l| !l.contains(math::TYPST)),
            "the tag itself is markdown, not content: {shown:?}"
        );
    }

    #[test]
    fn a_typst_formula_that_cannot_be_typeset_is_shown_as_the_block_it_was_written_as() {
        // D165: what cannot be drawn is drawn as its source. For a body in Typst's notation that
        // source is a block of Typst, so that is what is shown — never the TeX delimiters of the
        // other invocation, which would name a notation it was not written in.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries
            .push(Entry::Assistant("```typst-math\nzzz qqq\n```\n".into()));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert!(at.is_empty(), "nothing was typeset, so nothing is placed");
        let shown: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            shown.iter().any(|l| l.trim_end() == "  zzz qqq"),
            "{shown:?}"
        );
        assert!(
            shown
                .iter()
                .all(|l| !l.contains("$$") && !l.contains("\\[")),
            "not the other invocation's delimiters: {shown:?}"
        );
    }

    #[test]
    fn a_copy_of_a_typst_formula_is_the_fence_that_asks_for_it() {
        // D166: a Typst body is not LaTeX, so copying it in TeX's delimiters would paste it back as
        // letters. What travels with the copy is the invocation it was drawn from.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "so\n\n```typst-math\nfrac(a, b)\n```\n".into(),
        ));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.clone().expect("the transcript was drawn");
        assert_eq!(seen.pictures.len(), 1, "{:?}", seen.pictures);
        let last = seen.lines.len() - 1;
        app.selection = Some(Selection {
            anchor: Cell { line: 0, ch: 0 },
            focus: Cell { line: last, ch: 60 },
            dragging: false,
        });
        let text = app.selected_text().expect("a copy");
        assert!(text.contains("```typst-math\nfrac(a, b)\n```"), "{text:?}");
        assert!(!text.contains("\\["), "not TeX's delimiters: {text:?}");
    }

    #[test]
    fn a_copy_from_a_terminal_with_no_pictures_keeps_what_it_shows() {
        // No pictures means the source is on screen, and a copy of it is the text that is there: the
        // copy must not wrap delimiters around delimiters (D166).
        let mut app = bare_app();
        app.protocol = image::Protocol::Text;
        app.entries.push(Entry::Assistant("so $x^2$ holds".into()));
        app.cache.push(None);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app.seen.clone().expect("the transcript was drawn");
        assert!(seen.pictures.is_empty(), "{:?}", seen.pictures);
        let last = seen.lines.len() - 1;
        app.selection = Some(Selection {
            anchor: Cell { line: 0, ch: 0 },
            focus: Cell { line: last, ch: 60 },
            dragging: false,
        });
        let text = app.selected_text().expect("a copy");
        assert!(text.contains("so $x^2$ holds"), "{text:?}");
        assert!(!text.contains("\\("), "{text:?}");
    }

    #[test]
    fn a_formula_in_a_line_takes_cells_of_that_line() {
        // D165: a formula inside a sentence is drawn in cells the words around it leave, at the
        // column it starts at, and the row is the same width it was as source.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries
            .push(Entry::Assistant("if $x^2$ holds then".into()));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 1, "one formula, one picture");
        let At {
            line,
            column,
            picture,
            ..
        } = &at[0];
        assert_eq!(
            *column,
            Some(3),
            "`if ` is three cells: {:?}",
            plain(&lines[*line])
        );
        assert_eq!(picture.cells.rows, 1, "one row, whatever the formula is");
        assert!(
            picture.cells.cols > 1,
            "as wide as it is: {:?}",
            picture.cells
        );
        // The words are where they were, and nothing of the reservation is left on screen.
        let shown = plain(&lines[*line]);
        let words: Vec<&str> = shown.split_whitespace().collect();
        assert_eq!(words, ["if", "holds", "then"], "{shown:?}");
        assert!(!shown.contains(FORMULA) && !shown.contains(GUTTER));
        // And the marker is not anywhere else either.
        assert!(
            lines.iter().all(|l| !plain(l).contains(FORMULA)),
            "every formula was placed"
        );
    }

    #[test]
    fn a_formula_in_a_line_moves_to_the_next_row_whole() {
        // The run is measured like a word and cannot be broken: a formula that does not fit where
        // it was written goes to the next row in one piece, rather than being cut in half by the
        // wrapper (D165).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        // Eight words of four and a space fill 39 of the 40 columns, leaving room for no run at
        // all: the formula written after them has to move.
        let words = "1234 5678 9012 3456 7890 1234 5678 9012";
        app.entries
            .push(Entry::Assistant(format!("{words} $\\frac{{a}}{{b}}$ end")));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 40, false);
        assert_eq!(at.len(), 1, "one formula, one picture");
        let At {
            line,
            column,
            picture,
            ..
        } = &at[0];
        assert!(*line > 0, "it was moved off the row it was written on");
        assert_eq!(*column, Some(0), "to the start of the next row");
        let shown = plain(&lines[*line]);
        assert_eq!(
            shown.trim(),
            "end",
            "the words after it are on the row it moved to: {shown:?}"
        );
        assert!(
            shown.starts_with(&" ".repeat(picture.cells.cols as usize)),
            "the run takes its whole width on that row: {shown:?}"
        );
    }

    #[test]
    fn a_terminal_with_no_pictures_gets_an_inline_formula_as_its_source() {
        // D165: a formula whose picture cannot be drawn is drawn as the TeX it was written as, and
        // that holds for one inside a sentence as much as for one on its own line.
        let mut app = bare_app();
        app.protocol = image::Protocol::Text;
        app.entries
            .push(Entry::Assistant("if $x^2$ holds then".into()));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert!(at.is_empty(), "nothing to place");
        assert!(
            lines.iter().any(|l| plain(l).contains("$x^2$")),
            "the source is what is shown"
        );
    }

    #[test]
    fn a_formula_in_a_table_cell_stays_its_source() {
        // A table's columns are fitted rather than wrapped, so there is no run of cells for a
        // picture to sit in: it stays the TeX it was written as (D165).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "| a | b |\n|---|---|\n| $x^2$ | y |".into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert!(at.is_empty(), "nothing to place");
        assert!(
            lines.iter().any(|l| plain(l).contains("$x^2$")),
            "the source is what the table shows"
        );
    }

    #[test]
    fn the_theme_list_is_drawn_with_the_theme_it_offers() {
        // D74: the point of the picker is that you see the choice before making it, so the box has
        // to be drawn in the highlighted theme and say which one that is.
        let mut app = bare_app();
        let tables: ThemeTables = toml::from_str("[mine]\nborder = \"#ff0000\"\n").unwrap();
        let name = app.theme.name.clone();
        app.open_themes(ThemePicker::collect(&tables, &name));
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
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
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
            status: Template::default(),
            images: None,
            theme: None,
            notify: Notify::Off,
            math_scale: 1.0,
            keys: Keys::default(),
        })
    }

    #[test]
    fn the_overview_page_lists_the_team_and_shares_the_bus_and_queue() {
        // D195: ctrl+b opens a page with the sessions in a list on the left and the selected one
        // drawn large on the right, with the team's message bus and shared command queue as two
        // more rows of the same list.
        use crate::team::{Beat, BusMessage, QueueItem, QueueState, Render};
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        fn runtime() -> Render {
            Render {
                name: "runtime".into(),
                brief: "probe the build".into(),
                state: "running".into(),
                turns: 1,
                beats: vec![
                    Beat::Prompt("probe the build".into()),
                    Beat::Thinking("the reload path is here".into()),
                    Beat::Text("Looking at **the loader** now.".into()),
                    Beat::Call {
                        name: "bash".into(),
                        detail: "grep -n reload src".into(),
                        timeout: None,
                    },
                    Beat::Result {
                        ok: true,
                        content: "src/lib.rs:17: fn reload".into(),
                        diff: None,
                        images: Vec::new(),
                    },
                ],
                ..Render::default()
            }
        }
        let mut app = bare_app();
        // Reasoning is drawn as it streams, in the page and in a worker's own view alike.
        app.show_thinking = true;
        app.agent(AgentEvent::Text("the lead's own words".into()));
        app.agent(AgentEvent::Workers(vec![
            runtime(),
            Render {
                name: "tests".into(),
                state: "idle".into(),
                turns: 2,
                ..Render::default()
            },
        ]));
        app.agent(AgentEvent::Bus(vec![BusMessage {
            from: "lead".into(),
            to: "runtime".into(),
            text: "check the loader".into(),
        }]));
        app.agent(AgentEvent::Queue(vec![QueueItem {
            id: 1,
            command: "cargo test -p saddle-cap".into(),
            why: "not two at once".into(),
            owner: "lead".into(),
            state: QueueState::Pending,
        }]));
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(app.key(ctrl_b), Some(Action::Workers));
        assert_eq!(app.overview, Some(1), "opens on the running worker");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // The editor is behind the page, so a prompt typed there cannot be on screen while it is
        // open: the page owns the whole frame.
        app.editor.set("zzz-prompt".into());
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("agent sessions"), "{shown}");
        assert!(shown.contains("╭ sessions"), "{shown}");
        assert!(shown.contains("╭ runtime ● running"), "{shown}");
        assert!(shown.contains("tests"), "{shown}");
        assert!(shown.contains("bus (1)"), "{shown}");
        assert!(shown.contains("queue (1)"), "{shown}");
        // The selected session is drawn large beside the list: reasoning, markdown without its
        // markers, and the tool block with its call and answer.
        assert!(shown.contains("the reload path is here"), "{shown}");
        assert!(shown.contains("Looking at the loader now."), "{shown}");
        assert!(!shown.contains("**"), "markdown markers: {shown}");
        assert!(shown.contains("grep -n reload src"), "{shown}");
        assert!(shown.contains("src/lib.rs:17"), "{shown}");
        assert!(!shown.contains("zzz-prompt"), "{shown}");
        // The list walks the sessions and then the two boards, and the detail pane follows.
        assert_eq!(app.key(KeyEvent::from(KeyCode::Down)), None);
        assert_eq!(app.overview, Some(2), "the idle worker");
        app.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.overview, Some(3), "the bus");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let bus = screen(&terminal);
        assert!(bus.contains("lead → runtime"), "{bus}");
        assert!(bus.contains("check the loader"), "{bus}");
        app.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.overview, Some(4), "the queue");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let queue = screen(&terminal);
        assert!(queue.contains("#1"), "{queue}");
        assert!(queue.contains("pending"), "{queue}");
        assert!(queue.contains("cargo test -p saddle-cap"), "{queue}");
        // A board is read, not run: enter and the worker keys do nothing there, and a digit still
        // picks a session.
        assert_eq!(app.key(KeyEvent::from(KeyCode::Enter)), None);
        assert_eq!(app.overview, Some(4));
        assert_eq!(app.key(KeyEvent::from(KeyCode::Char('i'))), None);
        assert_eq!(app.key(KeyEvent::from(KeyCode::Char('x'))), None);
        assert_eq!(app.key(KeyEvent::from(KeyCode::Char('1'))), None);
        assert_eq!(app.overview, Some(1));
        // Enter on a worker opens its own full transcript, and esc returns to the page on it.
        assert_eq!(
            app.key(KeyEvent::from(KeyCode::Enter)),
            Some(Action::OpenWorker("runtime".into()))
        );
        app.worker = Some("runtime".into());
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let viewed = screen(&terminal);
        assert!(viewed.contains("agent transcript runtime"), "{viewed}");
        assert!(viewed.contains("esc/ctrl+b sessions"), "{viewed}");
        assert_eq!(
            app.key(KeyEvent::from(KeyCode::Esc)),
            Some(Action::WorkerBack)
        );
        assert_eq!(app.overview, Some(1));
        assert!(app.worker.is_none());
        // ctrl+b closes the page, back to the person's own conversation.
        assert_eq!(app.key(ctrl_b), None);
        assert!(app.overview.is_none());
        // With no workers yet the page still opens, with the person's own row and both empty
        // boards; `/workers` is the same page for a terminal that eats the key.
        let mut empty = bare_app();
        assert_eq!(empty.key(ctrl_b), Some(Action::Workers));
        assert_eq!(empty.overview, Some(0));
        terminal.draw(|frame| draw(frame, &mut empty)).unwrap();
        let none = screen(&terminal);
        assert!(none.contains("agent sessions"), "{none}");
        assert!(none.contains("╭ lead"), "{none}");
        assert!(none.contains("bus (0)"), "{none}");
        assert!(none.contains("queue (0)"), "{none}");
        assert!(none.contains("esc back"), "{none}");
        empty.overview = None;
        empty.editor.set("/workers".into());
        assert_eq!(
            empty.key(KeyEvent::from(KeyCode::Enter)),
            Some(Action::Workers)
        );
        assert_eq!(empty.overview, Some(0));
        // A prompt typed in a worker's own view goes to the worker, not the lead, and ctrl+b
        // there returns to the page on that worker.
        app.worker = Some("runtime".into());
        app.overview = None;
        app.editor.set("keep going".to_string());
        assert!(matches!(
            app.key(KeyEvent::from(KeyCode::Enter)),
            Some(Action::WorkerInput(_))
        ));
        assert_eq!(app.key(ctrl_b), Some(Action::WorkerBack));
        assert_eq!(app.overview, Some(1));
    }

    #[test]
    fn a_board_stays_selected_when_the_team_has_no_workers() {
        // The views event arrives on every agent-thread tick; bounding the selection by the team
        // (zero workers) pulled the bus and queue rows back to the lead at once (D195).
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = bare_app();
        app.overview = Some(0);
        app.agent(AgentEvent::Workers(Vec::new()));
        app.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.overview, Some(1));
        app.agent(AgentEvent::Workers(Vec::new()));
        assert_eq!(app.overview, Some(1), "the bus row keeps the selection");
        app.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.overview, Some(2));
        app.agent(AgentEvent::Bus(Vec::new()));
        app.agent(AgentEvent::Queue(Vec::new()));
        assert_eq!(app.overview, Some(2), "the queue row keeps the selection");
    }

    #[test]
    fn the_page_scrolls_a_sessions_history_and_a_boards_full_text() {
        // D195/D196: the detail is a session's own transcript from the start of what is kept, so
        // PgUp walks back through its turns and reasoning, and a board wraps its full text so a
        // long message or a multi-line command is readable rather than cut to a row.
        use crate::team::{Beat, BusMessage, QueueItem, QueueState, Render};
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = bare_app();
        app.show_thinking = true;
        let mut many = Render {
            name: "runtime".into(),
            state: "idle".into(),
            ..Render::default()
        };
        many.beats = (0..60)
            .map(|line| Beat::Text(format!("turn line {line}\n")))
            .collect();
        app.agent(AgentEvent::Workers(vec![many]));
        app.agent(AgentEvent::Bus(vec![BusMessage {
            from: "lead".into(),
            to: "runtime".into(),
            text: (0..60)
                .map(|line| format!("bus line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }]));
        app.agent(AgentEvent::Queue(vec![QueueItem {
            id: 7,
            command: "cargo test -p saddle-cap\ncargo test -p saddle-policy".into(),
            why: "one after the other".into(),
            owner: "lead".into(),
            state: QueueState::Pending,
        }]));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        app.overview = Some(1);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let tail = screen(&terminal);
        assert!(tail.contains("turn line 59"), "{tail}");
        app.key(KeyEvent::from(KeyCode::PageUp));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.worker_scroll > 0);
        let older = screen(&terminal);
        assert!(!older.contains("turn line 59"), "{older}");
        assert!(older.contains("PgUp/PgDn history"), "{older}");
        // The bus shows every line of the message; the bottom of the log is where it opens, and
        // Home takes the reader to its beginning.
        app.overview = Some(2);
        app.board_scroll = 0;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let bus = screen(&terminal);
        assert!(bus.contains("bus line 59"), "{bus}");
        app.key(KeyEvent::from(KeyCode::Home));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let oldest = screen(&terminal);
        assert!(oldest.contains("bus line 0"), "{oldest}");
        // A queued command's own newlines are kept, so a multi-command item is read as written.
        app.overview = Some(3);
        app.board_scroll = 0;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let queue = screen(&terminal);
        assert!(queue.contains("cargo test -p saddle-cap"), "{queue}");
        assert!(queue.contains("cargo test -p saddle-policy"), "{queue}");
        assert!(queue.contains("why: one after the other"), "{queue}");
    }

    #[test]
    fn reasoning_renders_markdown_emphasis_and_paragraphs_inside_its_gutter() {
        let theme = Theme::default();
        let reasoning = "***Testing the library***\n\nChecking **results**.\n\n- a long list entry that wraps onto another line";
        let rows = thinking_rows(reasoning, true, &theme, 30);
        let lines: Vec<_> = rows.iter().flat_map(|row| text(&wrap(row, 30))).collect();
        let shown = lines.join("\n");
        assert!(
            shown.contains("│ Testing the library\n│ \n│ Checking results."),
            "{shown}"
        );
        assert!(!shown.contains("***") && !shown.contains("**"), "{shown}");
        assert!(rows[0].spans.iter().any(|span| {
            span.content.contains("Testing")
                && span
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::ITALIC)
        }));
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("│ ") && line.width() <= 30)
        );
        let folded = thinking_rows(reasoning, false, &theme, 30);
        assert_eq!(folded.len(), 1);
        assert!(text(&wrap(&folded[0], 80))[0].contains("ctrl+t shows it"));
        let view = CompactionView {
            header: "earlier conversation".into(),
            reasoning: reasoning.into(),
            summary: String::new(),
            state: CompactState::Running,
            blocks: false,
        };
        // The reasoning is inside the entry's own toggle: it is drawn when the compaction is open,
        // not merely because reasoning is on (D124).
        let (rows, _) = compaction_block(&view, true, true, 0, &theme, 30);
        let shown = rows
            .iter()
            .flat_map(|row| text(&wrap(row, 30)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("│ Testing the library") && !shown.contains("***"),
            "{shown}"
        );
        let (rows, _) = compaction_block(&view, true, false, 0, &theme, 30);
        let folded = rows
            .iter()
            .flat_map(|row| text(&wrap(row, 30)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !folded.contains("│ Testing the library"),
            "a closed compaction showed its reasoning: {folded}"
        );
        assert!(folded.contains("∴ reasoned for 5 lines"), "{folded}");
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

        app.selection = Some(Selection {
            anchor: Cell { line: first, ch: 2 },
            focus: Cell {
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
        let mut app = bare_app();
        app.permissions = Some(Permissions::new(
            vec![
                PermissionRow {
                    rule: "agent.fs-read[0]".into(),
                    kind: Kind::FsRead,
                    root: "workspace".into(),
                    exclude: ".env*".into(),
                    on: true,
                    editable: true,
                    locked: None,
                },
                PermissionRow {
                    rule: "agent.exec".into(),
                    kind: Kind::Exec,
                    root: String::new(),
                    exclude: String::new(),
                    on: true,
                    editable: false,
                    locked: Some("exec is granted in the policy file".into()),
                },
            ],
            PermissionSource {
                policy_path: "~/.config/saddle/policies/home%me%proj.toml".into(),
                global_path: "~/.config/saddle/policy.toml".into(),
                text: "version = 1\n".into(),
                base: String::new(),
            },
            saddle_cap::Roots::default(),
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
    fn the_add_prompt_shows_the_entry_before_it_is_written() {
        // D173: the grant a line would add, and what it reaches, are on screen while it is typed.
        let mut app = bare_app();
        app.permissions = Some(Permissions::new(
            Vec::new(),
            PermissionSource {
                policy_path: "~/.config/saddle/policies/home%me%proj.toml".into(),
                global_path: "~/.config/saddle/policy.toml".into(),
                text: "version = 1\n".into(),
                base: "[agent]\nnet = [{ host = \"*\" }]\n".into(),
            },
            saddle_cap::Roots::default(),
        ));
        if let Some(dialog) = &mut app.permissions {
            dialog.begin_add();
            for c in "net https://api.example.com/v2/catalogue".chars() {
                dialog.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        for want in [
            "add  › net https://api.example.com/v2/catalogue",
            r#"[[agent.net]]  host = "api.example.com""#,
            r#"scheme = "https""#,
            "net grant for https://api.example.com:443",
            // The base grants a list this project's own would replace, which the entry cannot say.
            "own list replaces config.toml's 1 `net` entry",
            "enter add · tab complete · esc cancel",
        ] {
            assert!(all.contains(want), "missing {want:?} in\n{all}");
        }
        // A line that cannot be a grant says so instead, and nothing is drawn as if it could.
        if let Some(dialog) = &mut app.permissions {
            dialog.retry("net https://".into());
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(all.contains("needs a host"), "{all}");
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
        for name in theme::builtin_names() {
            let theme =
                theme::build(name, &ThemeTables::new()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(theme.name, name);
        }
        let light = theme::build("light", &ThemeTables::new()).unwrap();
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
        assert!(all.contains("conversation tree"), "{all}");
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
        // D168: the tree is the screen, not a window in it — its rows reach the last one.
        assert_eq!(
            app.dialog_seen.as_ref().map(|s| s.lines.len()),
            Some(20),
            "the tree drew {} rows of 20",
            app.dialog_seen.as_ref().map_or(0, |s| s.lines.len())
        );
        assert!(all.contains("5 messages"), "{all}");
    }

    #[test]
    fn the_tree_fills_the_screen_and_scrolls_inside_it() {
        // D168: the mode is the screen. A conversation longer than it scrolls with the keyboard, and
        // the list takes every row the title and the footers leave.
        let mut app = bare_app();
        let points = (1..=40)
            .map(|n| Point {
                node: n,
                parent: (n > 1).then(|| n - 1),
                kind: PointKind::Assistant,
                preview: format!("message {n}"),
                text: String::new(),
                here: n == 40,
                active: true,
            })
            .collect();
        let mut tree = Tree::new(points);
        tree.selected = 39;
        app.tree = Some(tree);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let seen = app
            .dialog_seen
            .as_ref()
            .expect("the tree is its own surface");
        assert_eq!(seen.lines.len(), 20, "every row of the screen");
        assert_eq!((seen.area.height, seen.top), (20, 0));
        // Title, a blank, fifteen rows of list, then the counts and the two key lines.
        assert_eq!(app.tree.as_ref().unwrap().rows, 15);
        assert!(
            seen.lines[0].contains("conversation tree"),
            "{:?}",
            seen.lines
        );
        assert!(
            seen.lines[16].contains("message 40"),
            "the keyboard's row is not the last row of the list: {:?}",
            seen.lines
        );
        assert!(seen.lines[16].starts_with('›'), "{:?}", seen.lines[16]);
        assert!(seen.lines[17].contains("40 messages"), "{:?}", seen.lines);
        assert!(seen.lines[19].contains("esc closes"), "{:?}", seen.lines);
        // Nothing of the transcript is left on the screen behind it.
        assert!(
            !seen.lines[1].contains("saddle ·"),
            "the header shows through: {:?}",
            seen.lines
        );
    }

    #[test]
    fn settings_and_resume_dialogs_draw() {
        let mut app = bare_app();
        app.settings = Some(SettingsDialog::new(Values {
            provider: "synthetic".into(),
            model: "hf:zai-org/GLM-5.3".into(),
            effort: Some("high".into()),
            max_tokens: None,
            context_window: None,
            window: 262_144,
            status: status::DEFAULT_STATUS.into(),
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
        let now = SystemTime::now();
        app.resume = Some(SessionPicker::new(vec![Summary {
            id: "a".into(),
            title: "fix the flaky test in the sandbox crate".into(),
            name: None,
            prompts: 3,
            messages: 9,
            model: "mock-model".into(),
            forked_from: None,
            worker: false,
            updated: now - Duration::from_secs(600),
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
    fn a_tools_question_is_the_end_of_the_transcript_with_subtext_and_a_recommendation() {
        let mut app = bare_app();
        let (asked, _answers) = Asked::pair(AskedQuestion {
            question: "Which approach should the loader take?".into(),
            choices: vec![
                AskChoice {
                    label: "rewrite it".into(),
                    description: "cleaner boundary".into(),
                    preview: "+ let host = prepare(component)?;\n- let host = legacy(component)?;"
                        .into(),
                    recommended: true,
                },
                AskChoice {
                    label: "patch it".into(),
                    description: String::new(),
                    preview: String::new(),
                    recommended: false,
                },
            ],
            allow_other: true,
            allow_note: true,
        });
        app.ask = Some(AskDialog::new(asked));
        // A conversation long enough to scroll, with the line that must stay visible just above
        // the question at its end.
        for line in 0..30 {
            app.note(NoteKind::Info, &format!("line {line} of what was said"));
        }
        app.note(
            NoteKind::Info,
            "a line of the conversation that must stay visible",
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        // The question is the transcript's last rows: what was said above it is still there, and
        // the frame carries its keys because the question itself can be scrolled away (D197).
        assert!(
            all.contains("a line of the conversation that must stay visible"),
            "the transcript was covered:\n{all}"
        );
        assert!(
            all.contains("↑↓ choose · 1-9 jump · enter answer · n note · esc dismiss"),
            "the keys are on the frame's own line:\n{all}"
        );
        for want in [
            "Question",
            "Which approach should the loader take?",
            "› 1. rewrite it",
            "(recommended)",
            "cleaner boundary",
            "2. patch it",
            "3. Type something…",
            "an answer of your own",
            "preview",
            "+ let host = prepare(component)?;",
            "↑↓ choose · 1-9 jump · enter answer · n note · esc dismiss",
        ] {
            assert!(all.contains(want), "missing {want:?} in\n{all}");
        }

        // The label is the theme's text; its subtext is the muted colour, so the two read apart.
        let row_of = |needle: &str| {
            let buf = terminal.backend().buffer();
            (0..buf.area.height).find(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(needle)
            })
        };
        let subtext = row_of("cleaner boundary").expect("the subtext row");
        let below = row_of("Which approach").expect("the question row");
        let said = row_of("a line of the conversation").expect("the transcript line");
        assert!(
            below > said && subtext > below,
            "the question is below the conversation, and its subtext below its label"
        );
        let buf = terminal.backend().buffer();
        let col = (0..buf.area.width)
            .find(|x| buf[(*x, subtext)].symbol() == "c")
            .unwrap();
        assert_eq!(
            buf[(col, subtext)].style().fg,
            Some(app.theme.muted),
            "the subtext is drawn in the quiet colour"
        );

        // `n` opens a note on the choice under the cursor, and the cursor sits on that line.
        if let Some(dialog) = &mut app.ask {
            dialog.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
            assert!(matches!(dialog.stage, AskStage::Noting { .. }));
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(all.contains("note ›"), "{all}");
        assert!(
            all.contains("note on \"rewrite it\" · enter send"),
            "the frame's line names the choice the note belongs to and what enter does: {all}"
        );
        assert!(
            all.contains("a line of the conversation that must stay visible"),
            "the note line did not cover the transcript either:\n{all}"
        );

        // Scrolling walks the transcript, and the question walks off the bottom with it: reading
        // what led to a question is the same gesture as ever (D197).
        for _ in 0..4 {
            app.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(app.scroll > 0, "the transcript scrolled under the question");
        assert!(
            !all.contains("Which approach should the loader take?"),
            "the question scrolled out of view rather than staying pinned:\n{all}"
        );
        assert!(
            all.contains("end to see it"),
            "and the frame says where it is:\n{all}"
        );
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains("Which approach should the loader take?"));
    }

    #[test]
    fn moving_through_a_questions_choices_keeps_its_question_and_choices_on_the_screen() {
        let mut app = bare_app();
        // The shape that got this wrong: three choices whose previews are long enough that the
        // block is taller than the screen, walked with the keys the question uses (D197).
        let (asked, _answers) = Asked::pair(AskedQuestion {
            question: "Where should the words you type go?".into(),
            choices: (1..=3)
                .map(|n| AskChoice {
                    label: format!("choice {n}"),
                    description: "a line about it".into(),
                    preview: (0..14)
                        .map(|line| format!("preview line {line} of choice {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    recommended: n == 1,
                })
                .collect(),
            allow_other: true,
            allow_note: true,
        });
        app.ask = Some(AskDialog::new(asked));
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        for turn in 0..8 {
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let all = screen(&terminal);
            assert!(
                all.contains("Where should the words you type go?"),
                "turn {turn} lost the question itself:\n{all}"
            );
            assert!(
                all.contains("1. choice 1") && all.contains("3. choice 3"),
                "turn {turn} lost the choices:\n{all}"
            );
            assert_eq!(app.scroll, 0, "turn {turn} scrolled the transcript");
            if turn == 0 {
                // The preview gives way rather than the question: it is cut, and says how much it
                // left out.
                assert!(
                    all.contains("more line"),
                    "the preview was cut short:\n{all}"
                );
            }
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }

        // `n` opens the note of the choice under the cursor, and the frame names the choice it
        // belongs to, so an answer is never sent by a keystroke that meant to add to it (D197).
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let all = screen(&terminal);
        assert!(
            all.contains("note ›"),
            "the note line is on the screen:\n{all}"
        );
        assert!(
            all.contains("note on \"choice"),
            "and the frame says which choice it is for:\n{all}"
        );
        assert!(all.contains("empty note is none"), "{all}");
        assert!(
            all.contains("Where should the words you type go?"),
            "the question is still readable while its note is written:\n{all}"
        );
    }

    #[test]
    fn a_questions_block_keeps_one_height_and_puts_the_cursor_where_the_note_is() {
        let theme = Theme::default();
        let (asked, _answers) = Asked::pair(AskedQuestion {
            question: "which one?".into(),
            choices: vec![
                AskChoice {
                    label: "short".into(),
                    description: "a line".into(),
                    preview: "one line".into(),
                    recommended: false,
                },
                AskChoice {
                    label: "long".into(),
                    description: String::new(),
                    preview: (0..12)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    recommended: true,
                },
                AskChoice {
                    label: "none".into(),
                    description: String::new(),
                    preview: String::new(),
                    recommended: false,
                },
            ],
            allow_other: true,
            allow_note: true,
        });
        let mut dialog = AskDialog::new(asked);
        // The room the preview takes belongs to the question, not to the choice under the cursor:
        // walking the choices must not change the block's height and shift the screen (D197).
        let mut heights = Vec::new();
        for row in 0..dialog.rows() {
            dialog.selected = row;
            heights.push(ask_rows(&dialog, 80, Some(24), &theme).0.len());
        }
        assert!(
            heights.windows(2).all(|pair| pair[0] == pair[1]),
            "the block changed height while the choices were walked: {heights:?}"
        );
        // Including the rows the note and the error add, and the same with no room to spare.
        dialog.stage = AskStage::Noting {
            choice: 0,
            text: "half".into(),
        };
        let (lines, cursor) = ask_rows(&dialog, 80, Some(24), &theme);
        let (row, col) = cursor.expect("a note has a cursor");
        let text = plain(&lines[row]);
        let written = "  note › half";
        assert!(
            text.starts_with(written),
            "the cursor is on the note line: {text:?}"
        );
        assert_eq!(
            col,
            written.width(),
            "the cursor follows the words, never past the edge"
        );
        assert!(col < 80);
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
            status: Template::default(),
            images: None,
            theme: None,
            notify: Notify::Off,
            math_scale: 1.0,
            keys: Keys::default(),
        });
        app.entries.push(Entry::User("fix the build".into()));
        app.cache.push(None);
        app.busy = Some(Activity::Waiting);
        app.agent(AgentEvent::Text("Looking at **it**.".into()));
        app.agent(AgentEvent::Call {
            id: "test-call".into(),
            name: "bash".into(),
            detail: "cargo build".into(),
            timeout: None,
        });
        app.agent(AgentEvent::Result {
            images: Vec::new(),
            ok: false,
            content: "denied: agent has no exec grant".into(),
            diff: None,
        });
        app.agent(AgentEvent::Reply {
            usage: Usage {
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
        // Inspect the painted glyph, not just its text: matching foreground/background hid errors.
        let column = screen[row as usize].find("denied:").unwrap() as u16;
        assert_eq!(buf[(column, row)].fg, Theme::default().error);
        assert_ne!(buf[(column, row)].fg, buf[(column, row)].bg);
        assert_eq!(terminal.backend().cursor_position(), Position::new(5, 17));
    }
    #[test]
    fn background_blocks_use_pending_until_host_completion_and_distinguish_failure() {
        let theme = Theme::default();
        for (state, expected) in [
            (TaskState::Running, theme.tool_pending_bg),
            (TaskState::Succeeded, theme.tool_success_bg),
            (TaskState::Failed, theme.tool_error_bg),
            (TaskState::Cancelled, theme.tool_error_bg),
            (TaskState::TimedOut, theme.tool_error_bg),
        ] {
            let (mut rows, mut bg) = tool_block(
                "bash",
                "sleep 9",
                Some(&(true, "launch succeeded".into())),
                None,
                &view(false),
                Timing {
                    elapsed: None,
                    limit: None,
                },
                &theme,
            );
            let now = Instant::now();
            background_block(
                &mut rows,
                &mut bg,
                &[TaskView {
                    id: "task-host".into(),
                    call: Some("call".into()),
                    state,
                    started: now - Duration::from_secs(5),
                    stopped: Some(now - Duration::from_secs(3)),
                }],
                0,
                now,
                &theme,
            );
            assert_eq!(bg, expected);
            let line = text(&wrap(&rows[0], 200)).join(" ");
            assert!(line.contains(state.label()), "{line}");
            // Each task says how long it ran, from its own start to its own stop (D172).
            assert!(line.contains("2.0s"), "{line}");
        }
    }

    #[test]
    fn a_formula_with_a_transpose_in_it_is_drawn_rather_than_shown_as_its_source() {
        // Reported from a real terminal: two of the formulas in one paragraph were left as their
        // source while the ones around them were typeset. The whole formula is refused when one
        // command is not translated (`\top` was not), so a single absent symbol costs the picture
        // — and the paragraph reads as if `$$` and `$…$` were not supported at all (D165).
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "a push moves it by \\(\\delta\\). The very same fixed rule\nbecomes\n$$\nr^\\top \\delta > b-r^\\top h_q.\n$$\n\nand the rule could be $r^\\top h>b(c)$ itself".into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 3, "one in a line, one block, one more in a line");
        assert!(
            lines.iter().all(|l| !plain(l).contains("\\top")),
            "the source is not what is shown: {:?}",
            lines.iter().map(plain).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_rate_bound_with_a_slanted_inequality_is_drawn_rather_than_shown_as_its_source() {
        // Reported from a real terminal: `$k \epsilon^2 \gtrsim \log n$` came out as its source,
        // because `\gtrsim` had no Typst name and one untranslated command refuses the whole
        // formula (D165). The slanted inequality is a glyph Typst has, so the line is a picture.
        let mut app = bare_app();
        app.protocol = image::Protocol::Kitty;
        app.entries.push(Entry::Assistant(
            "the gap is $k \\epsilon^2 \\gtrsim \\log n$ here".into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert_eq!(at.len(), 1, "the formula is a picture in the line");
        assert!(
            lines.iter().all(|l| !plain(l).contains("gtrsim")),
            "the source is not what is shown: {:?}",
            lines.iter().map(plain).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_display_formula_left_to_its_source_keeps_the_lines_it_was_written_on() {
        // A row of text holds no line break — the drawing pass drops a control character — so a
        // source written over three lines used to reach the screen as one line. The shape a writer
        // gives a long formula is what makes its source readable (D165).
        let mut app = bare_app();
        app.protocol = image::Protocol::Text;
        app.entries.push(Entry::Assistant(
            "becomes\n$$\nr^\\foo > b\n\\qquad \\bar{y} < c\n$$".into(),
        ));
        app.cache.push(None);
        let (lines, at) = transcript_with_images(&mut app, 80, false);
        assert!(at.is_empty(), "a terminal with no pictures places none");
        let shown: Vec<String> = lines
            .iter()
            .map(|l| plain(l).trim_end().to_owned())
            .collect();
        assert!(
            shown.ends_with(&[
                "$$".to_owned(),
                "r^\\foo > b".to_owned(),
                "\\qquad \\bar{y} < c".to_owned(),
                "$$".to_owned(),
            ]),
            "{shown:?}"
        );
    }
}
