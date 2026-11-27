use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose;
use png::BitDepth;
use png::ColorType;
use png::Encoder;
use typst::Library;
use typst::LibraryExt;
use typst::World;
use typst::diag::FileError;
use typst::diag::FileResult;
use typst::foundations::Bytes;
use typst::foundations::Datetime;
use typst::foundations::Duration;
use typst::syntax::FileId;
use typst::syntax::RootedPath;
use typst::syntax::Source;
use typst::syntax::VirtualPath;
use typst::syntax::VirtualRoot;
use typst::text::Font;
use typst::text::FontBook;
use typst::utils::LazyHash;
use typst::utils::Scalar;
use typst_layout::PagedDocument;
use typst_render::RenderOptions;
use typst_render::render as rasterise;

use crate::provider::Image;

const FONT_PT: f32 = 11.0;

const LINE_PT: f32 = 13.2;

const DISPLAY: f32 = 1.15;

const X_HEIGHT: f32 = 1.2;

const FLOOR: f32 = 0.55;

const PAD_X_PT: f32 = 2.0;
const PAD_TOP_PT: f32 = 1.0;
const PAD_BOTTOM_PT: f32 = 7.0;

const RESOLUTION: u32 = 2;

const MAX_PIXELS: f64 = 4_000_000.0;

const MIN_SCALE: f32 = 0.5;
const MAX_SCALE: f32 = 4.0;

const INLINE_BASELINE: f32 = 0.75;

const INLINE_PAD_X_PT: f32 = 1.5;

const INLINE_STRUT_PT: f32 = 32.0;

pub struct Room {
    pub cell: (u32, u32),
    pub cols: u16,
    pub rows: u16,
    pub colour: (u8, u8, u8),
    pub scale: f32,
}

impl Room {
    #[cfg(test)]
    pub fn tiny() -> Self {
        Self {
            cell: (8, 16),
            cols: 2,
            rows: 1,
            scale: 1.0,
            colour: (0, 0, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notation {
    Latex,
    Typst,
}

pub const TYPST: &str = "typst-math";

#[derive(Clone, Debug)]
pub struct Formula {
    pub image: Image,
    pub cells: (u16, u16),
    pub inline: bool,
    pub notation: Notation,
    pub source: String,
}

pub fn render(source: &str, room: &Room) -> Option<Formula> {
    let body = latex(source)?;
    place(&body, source, Notation::Latex, room, false)
}

pub fn render_typst(source: &str, room: &Room) -> Option<Formula> {
    place(source.trim(), source, Notation::Typst, room, false)
}

fn place(
    body: &str,
    source: &str,
    notation: Notation,
    room: &Room,
    inline: bool,
) -> Option<Formula> {
    let key = (
        body.to_owned(),
        room.cell,
        room.cols,
        room.rows,
        room.colour,
        room.scale.to_bits(),
        inline,
    );
    if let Some(mut formula) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        formula.source = source.to_owned();
        formula.notation = notation;
        return Some(formula);
    }
    let mut formula = if inline {
        typeset_inline(body, room)?
    } else {
        typeset(body, room)?
    };
    formula.source = source.to_owned();
    formula.notation = notation;
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key, formula.clone());
    }
    Some(formula)
}

fn cache() -> &'static Mutex<HashMap<Key, Formula>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Formula>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

type Key = (String, (u32, u32), u16, u16, (u8, u8, u8), u32, bool);

const MAIN: &str = "/equation.typ";

fn typeset(body: &str, room: &Room) -> Option<Formula> {
    let document = compile(body, room, false)?;
    let page = document.pages().first()?;
    let size = page.frame.size();
    let (width_pt, height_pt) = (size.x.to_pt(), size.y.to_pt());
    if width_pt <= 0.0 || height_pt <= 0.0 {
        return None;
    }
    let (cell_w, cell_h) = (room.cell.0.max(1) as f64, room.cell.1.max(1) as f64);
    let chosen = DISPLAY as f64 * X_HEIGHT as f64 * room.scale.clamp(MIN_SCALE, MAX_SCALE) as f64;
    let natural = cell_h / LINE_PT as f64;
    let mut scale = chosen * natural;
    scale = scale.min(room.cols.max(1) as f64 * cell_w / width_pt);
    scale = scale.min(room.rows.max(1) as f64 * cell_h / height_pt);
    if scale < natural * FLOOR as f64 {
        return None;
    }
    let cells = (
        ((width_pt * scale / cell_w).round() as u16).max(1),
        ((height_pt * scale / cell_h).round() as u16).max(1),
    );
    let refine = (scale * RESOLUTION as f64).min((MAX_PIXELS / (width_pt * height_pt)).sqrt());
    let pixmap = rasterise(
        page,
        &RenderOptions {
            pixel_per_pt: Scalar::new(refine),
            render_bleed: false,
        },
    );
    let (width, height) = (pixmap.width(), pixmap.height());
    if width == 0 || height == 0 {
        return None;
    }
    let bytes = encode_png(pixmap.width(), pixmap.height(), pixmap.data())?;
    Some(Formula {
        cells,
        inline: false,
        notation: Notation::Latex,
        source: String::new(),
        image: Image {
            media_type: "image/png".to_owned(),
            data: general_purpose::STANDARD.encode(&bytes),
            name: "equation".to_owned(),
            width,
            height,
        },
    })
}

pub fn render_inline(source: &str, room: &Room) -> Option<Formula> {
    let body = latex(source)?;
    place(&body, source, Notation::Latex, room, true)
}

fn typeset_inline(body: &str, room: &Room) -> Option<Formula> {
    let document = compile(body, room, true)?;
    let page = document.pages().first()?;
    let size = page.frame.size();
    let (width_pt, height_pt) = (size.x.to_pt() as f32, size.y.to_pt() as f32);
    if width_pt <= 0.0 {
        return None;
    }
    if (height_pt - 2.0 * INLINE_STRUT_PT).abs() > 0.01 {
        return None;
    }
    let (cell_w, cell_h) = (room.cell.0.max(1) as f32, room.cell.1.max(1) as f32);
    let natural = LINE_PT / X_HEIGHT;
    let probe = cell_h / natural * RESOLUTION as f32;
    let pixmap = rasterise(
        page,
        &RenderOptions {
            pixel_per_pt: Scalar::new(probe as f64),
            render_bleed: false,
        },
    );
    let (page_px, page_rows) = (pixmap.width(), pixmap.height());
    if page_px == 0 || page_rows == 0 {
        return None;
    }
    let baseline_px = INLINE_STRUT_PT * probe;
    let (ink_top, ink_bottom) = ink_rows(pixmap.data(), page_px, page_rows)?;
    let ascent = (baseline_px - ink_top as f32) / probe;
    let descent = (ink_bottom as f32 + 1.0 - baseline_px) / probe;
    let below = 1.0 - INLINE_BASELINE;
    let need = (ascent / INLINE_BASELINE).max(descent / below);
    let height_box = need.max(natural) * room.scale.clamp(MIN_SCALE, MAX_SCALE);
    if natural / height_box < FLOOR {
        return None;
    }
    let box_scale = cell_h / height_box;
    let cols = ((width_pt * box_scale / cell_w).ceil() as u16).max(1);
    if cols > room.cols.max(1) * 3 / 4 {
        return None;
    }
    let top_px = (baseline_px - INLINE_BASELINE * height_box * probe)
        .round()
        .max(0.0) as u32;
    let rows_px = (height_box * probe).round().max(1.0) as u32;
    if top_px + rows_px > page_rows {
        return None;
    }
    let width_px = ((cols as f32 * cell_w * (probe / box_scale)).round() as u32).max(page_px);
    let framed = frame(pixmap.data(), page_px, top_px, rows_px, width_px)?;
    let bytes = encode_png(width_px, rows_px, &framed)?;
    Some(Formula {
        cells: (cols, 1),
        inline: true,
        notation: Notation::Latex,
        source: String::new(),
        image: Image {
            media_type: "image/png".to_owned(),
            data: general_purpose::STANDARD.encode(&bytes),
            name: "equation".to_owned(),
            width: width_px,
            height: rows_px,
        },
    })
}

fn ink_rows(data: &[u8], width: u32, height: u32) -> Option<(u32, u32)> {
    let stride = width as usize * 4;
    let mut first = None;
    let mut last = 0;
    for y in 0..height as usize {
        let row = &data[y * stride..(y + 1) * stride];
        if row.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 8) {
            first.get_or_insert(y as u32);
            last = y as u32;
        }
    }
    first.map(|first| (first, last))
}

fn frame(data: &[u8], from: u32, top: u32, rows: u32, width: u32) -> Option<Vec<u8>> {
    if width < from {
        return None;
    }
    let (stride, to) = (from as usize * 4, width as usize * 4);
    let left = (to - stride) / 8 * 4;
    let mut out = vec![0u8; to * rows as usize];
    for y in 0..rows as usize {
        let source = data.get((top as usize + y) * stride..(top as usize + y + 1) * stride)?;
        out[y * to + left..y * to + left + stride].copy_from_slice(source);
    }
    Some(out)
}

fn compile(body: &str, room: &Room, inline: bool) -> Option<PagedDocument> {
    layout(body, room, inline)
}

fn layout(body: &str, room: &Room, inline: bool) -> Option<PagedDocument> {
    let (r, g, b) = room.colour;
    let text = if inline {
        let strut = INLINE_STRUT_PT * 2.0;
        format!(
            "#set page(width: auto, height: auto, margin: (x: {INLINE_PAD_X_PT}pt, top: 0pt, bottom: 0pt), fill: none)\n\
             #set text(size: {FONT_PT}pt, fill: rgb({r}, {g}, {b}))\n\
             #box(width: 0pt, height: {strut}pt, baseline: {INLINE_STRUT_PT}pt)${body}$\n"
        )
    } else {
        format!(
            "#set page(width: auto, height: auto, margin: (x: {PAD_X_PT}pt, top: {PAD_TOP_PT}pt, bottom: {PAD_BOTTOM_PT}pt), fill: none)\n\
             #set text(size: {FONT_PT}pt, fill: rgb({r}, {g}, {b}))\n\
             $ {body} $\n"
        )
    };
    let world = Snippet::new(&text)?;
    typst::compile::<PagedDocument>(&world).output.ok()
}

fn encode_png(width: u32, height: u32, data: &[u8]) -> Option<Vec<u8>> {
    let mut rgba = data.to_vec();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = pixel[3] as u32;
        if alpha == 0 {
            pixel.fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(out)
}

struct Snippet {
    main: FileId,
    source: Source,
    library: LazyHash<Library>,
    book: &'static LazyHash<FontBook>,
    fonts: &'static [Font],
}

impl Snippet {
    fn new(text: &str) -> Option<Self> {
        let (book, fonts) = fonts();
        let main = FileId::new(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(MAIN).ok()?,
        ));
        Some(Self {
            main,
            source: Source::new(main, text.to_owned()),
            library: LazyHash::new(Library::default()),
            book,
            fonts,
        })
    }
}

fn fonts() -> (&'static LazyHash<FontBook>, &'static [Font]) {
    static FONTS: OnceLock<(LazyHash<FontBook>, Vec<Font>)> = OnceLock::new();
    let (book, fonts) = FONTS.get_or_init(|| {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();
        for data in typst_assets::fonts() {
            for font in Font::iter(Bytes::new(data.to_vec())) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }
        (LazyHash::new(book), fonts)
    });
    (book, fonts.as_slice())
}

impl World for Snippet {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(MAIN.into()))
        }
    }

    fn file(&self, _id: FileId) -> FileResult<Bytes> {
        Err(FileError::NotFound(MAIN.into()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

#[derive(Default)]
struct Emitted {
    text: String,
    name: bool,
}

impl Emitted {
    fn name(&mut self, name: &str) {
        let joins = self.name
            || self
                .text
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if joins
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            self.text.push(' ');
        }
        self.text.push_str(name);
        self.name = true;
    }

    fn chr(&mut self, c: char) {
        if self.name && c.is_ascii_alphanumeric() {
            self.text.push(' ');
        }
        self.text.push(c);
        self.name = false;
    }
}

fn emit(out: &mut Emitted, text: &str) {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => out.chr(c),
        _ => out.name(text),
    }
}

pub fn latex(source: &str) -> Option<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Emitted::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_alphabetic() {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                i = end;
                if name.is_empty() {
                    let Some(next) = chars.get(i) else {
                        break;
                    };
                    i += 1;
                    let text = match next {
                        '\\' => "\\",
                        ',' | ';' | ':' | ' ' | '!' => " ",
                        '{' => "brace.l",
                        '}' => "brace.r",
                        '|' => "bar.v.double",
                        '%' => "percent",
                        '#' => "hash",
                        '$' => "dollar",
                        '&' => "&",
                        '_' => "_",
                        '^' => "^",
                        _ => return None,
                    };
                    emit(&mut out, text);
                    continue;
                }
                if !command(&name, &chars, &mut i, &mut out) {
                    return None;
                }
            }
            '{' => {
                emit(&mut out, "(");
                i += 1;
            }
            '}' => {
                emit(&mut out, ")");
                i += 1;
            }
            '~' => {
                emit(&mut out, " ");
                i += 1;
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let mut run = String::new();
                for (n, letter) in chars[start..i].iter().enumerate() {
                    if n > 0 {
                        run.push(' ');
                    }
                    run.push(*letter);
                }
                emit(&mut out, &run);
            }
            _ => {
                emit(&mut out, &c.to_string());
                i += 1;
            }
        }
    }
    Some(out.text.trim().to_owned())
}

fn command(name: &str, chars: &[char], i: &mut usize, out: &mut Emitted) -> bool {
    if let Some(symbol) = symbol(name) {
        emit(out, symbol);
        return true;
    }
    match name {
        "frac" | "dfrac" | "tfrac" => {
            let Some(a) = argument(chars, i) else {
                return false;
            };
            let Some(b) = argument(chars, i) else {
                return false;
            };
            emit(out, &format!("frac({a}, {b})"));
            true
        }
        "binom" => {
            let Some(a) = argument(chars, i) else {
                return false;
            };
            let Some(b) = argument(chars, i) else {
                return false;
            };
            emit(out, &format!("binom({a}, {b})"));
            true
        }
        "sqrt" => {
            let index = if chars.get(*i) == Some(&'[') {
                *i += 1;
                let mut depth = 1;
                let mut index = String::new();
                while let Some(&c) = chars.get(*i) {
                    *i += 1;
                    match c {
                        '[' => depth += 1,
                        ']' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    index.push(c);
                }
                let Some(index) = latex(&index) else {
                    return false;
                };
                Some(index)
            } else {
                None
            };
            let Some(x) = argument(chars, i) else {
                return false;
            };
            match index {
                Some(index) => emit(out, &format!("root({index}, {x})")),
                None => emit(out, &format!("sqrt({x})")),
            }
            true
        }
        "text" | "textrm" | "mathrm" | "operatorname" | "mbox" => {
            let Some(text) = literal(chars, i) else {
                return false;
            };
            match name {
                "text" | "textrm" | "mbox" => emit(out, &format!("\"{}\"", escape(&text))),
                "operatorname" => emit(out, &format!("op(\"{}\")", escape(&text))),
                _ => emit(out, &format!("op(\"{}\")", escape(&text))),
            }
            true
        }
        "mathbb" | "mathbf" | "mathcal" | "mathfrak" | "mathit" | "mathsf" | "mathtt" => {
            let Some(text) = argument(chars, i) else {
                return false;
            };
            let style = match name {
                "mathbb" => "bb",
                "mathbf" => "bold",
                "mathcal" => "cal",
                "mathfrak" => "frak",
                "mathit" => "italic",
                "mathsf" => "sans",
                _ => "mono",
            };
            emit(out, &format!("{style}({text})"));
            true
        }
        "hat" | "widehat" | "vec" | "bar" | "overline" | "underline" | "tilde" | "widetilde"
        | "dot" | "ddot" => {
            let Some(text) = argument(chars, i) else {
                return false;
            };
            let accent = match name {
                "hat" | "widehat" => "hat",
                "vec" => "arrow",
                "bar" | "overline" => "overline",
                "underline" => "underline",
                "tilde" | "widetilde" => "tilde",
                "dot" => "dot",
                _ => "dot.double",
            };
            emit(out, &format!("{accent}({text})"));
            true
        }
        "begin" => environment(chars, i, out),
        "left" | "right" => {
            let Some(delimiter) = delimiter(chars, i) else {
                return false;
            };
            emit(out, &delimiter);
            true
        }
        "displaystyle" | "textstyle" | "scriptstyle" | "limits" | "nolimits" => true,
        "quad" => {
            emit(out, "quad");
            true
        }
        "qquad" => {
            emit(out, "wide");
            true
        }
        _ => false,
    }
}

fn environment(chars: &[char], i: &mut usize, out: &mut Emitted) -> bool {
    let Some(name) = literal(chars, i) else {
        return false;
    };
    let name = name.trim();
    let mut body = String::new();
    let mut depth = 1;
    let mut j = *i;
    while j < chars.len() {
        if chars[j] == '\\' {
            let start = j + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            let word: String = chars[start..end].iter().collect();
            if word == "end" {
                let mut k = end;
                if chars.get(k) == Some(&'{') {
                    k += 1;
                    let mut closing = String::new();
                    while let Some(&c) = chars.get(k) {
                        k += 1;
                        if c == '}' {
                            break;
                        }
                        closing.push(c);
                    }
                    if closing.trim() == name {
                        depth -= 1;
                        if depth == 0 {
                            j = k;
                            break;
                        }
                    }
                }
            } else if word == "begin" {
                depth += 1;
            }
        }
        body.push(chars[j]);
        j += 1;
    }
    *i = j;
    let Some(cells) = body
        .split("\\\\")
        .map(|row| row.split('&').map(latex).collect::<Option<Vec<String>>>())
        .collect::<Option<Vec<Vec<String>>>>()
    else {
        return false;
    };
    let rows = cells
        .iter()
        .map(|row| row.join(", "))
        .collect::<Vec<_>>()
        .join("; ");
    match name {
        "matrix" => out.name(&format!("mat({rows})")),
        "pmatrix" => out.name(&format!("mat(delim: \"(\", {rows})")),
        "bmatrix" => out.name(&format!("mat(delim: \"[\", {rows})")),
        "vmatrix" => out.name(&format!("mat(delim: \"|\", {rows})")),
        "cases" => {
            let rows: Vec<String> = cells.iter().map(|row| row.join(" & ")).collect();
            out.name(&format!("cases({})", rows.join(", ")))
        }
        "aligned" | "align" | "align*" | "gathered" | "split" => {
            let Some(body) = latex(&body) else {
                return false;
            };
            out.name(&body);
        }
        "equation" | "equation*" | "displaymath" => {
            let Some(body) = latex(&body) else {
                return false;
            };
            out.name(&body);
        }
        _ => return false,
    }
    true
}

fn literal(chars: &[char], i: &mut usize) -> Option<String> {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
    match chars.get(*i)? {
        '{' => {
            *i += 1;
            let mut depth = 1;
            let mut text = String::new();
            while let Some(&c) = chars.get(*i) {
                *i += 1;
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                text.push(c);
            }
            Some(text)
        }
        &c => {
            *i += 1;
            Some(c.to_string())
        }
    }
}

fn delimiter(chars: &[char], i: &mut usize) -> Option<String> {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
    match chars.get(*i)? {
        '.' => {
            *i += 1;
            Some(String::new())
        }
        '\\' => {
            let start = *i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            let word: String = chars[start..end].iter().collect();
            *i = end;
            if !word.is_empty() {
                return symbol(&word).map(str::to_owned);
            }
            let next = *chars.get(*i)?;
            *i += 1;
            match next {
                '{' => Some("brace.l".to_owned()),
                '}' => Some("brace.r".to_owned()),
                '|' => Some("bar.v.double".to_owned()),
                _ => None,
            }
        }
        &c => {
            *i += 1;
            Some(c.to_string())
        }
    }
}

fn argument(chars: &[char], i: &mut usize) -> Option<String> {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
    match chars.get(*i)? {
        '{' | '(' => {
            let open = chars[*i];
            let close = if open == '{' { '}' } else { ')' };
            *i += 1;
            let mut depth = 1;
            let mut body = String::new();
            while let Some(&c) = chars.get(*i) {
                *i += 1;
                if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return latex(&body);
                    }
                }
                body.push(c);
            }
            None
        }
        '\\' => {
            let start = *i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            let word = chars[start..end].iter().collect::<String>();
            *i = end;
            let mut out = Emitted::default();
            if !command(&word, chars, i, &mut out) {
                return None;
            }
            Some(out.text)
        }
        &c => {
            *i += 1;
            Some(c.to_string())
        }
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "alpha",
        "beta" => "beta",
        "gamma" => "gamma",
        "delta" => "delta",
        "epsilon" => "epsilon",
        "varepsilon" => "epsilon.alt",
        "zeta" => "zeta",
        "eta" => "eta",
        "theta" => "theta",
        "vartheta" => "theta.alt",
        "iota" => "iota",
        "kappa" => "kappa",
        "lambda" => "lambda",
        "mu" => "mu",
        "nu" => "nu",
        "xi" => "xi",
        "pi" => "pi",
        "varpi" => "pi.alt",
        "rho" => "rho",
        "sigma" => "sigma",
        "tau" => "tau",
        "upsilon" => "upsilon",
        "phi" => "phi",
        "varphi" => "phi.alt",
        "chi" => "chi",
        "psi" => "psi",
        "omega" => "omega",
        "Gamma" => "Gamma",
        "Delta" => "Delta",
        "Theta" => "Theta",
        "Lambda" => "Lambda",
        "Xi" => "Xi",
        "Pi" => "Pi",
        "Sigma" => "Sigma",
        "Upsilon" => "Upsilon",
        "Phi" => "Phi",
        "Psi" => "Psi",
        "Omega" => "Omega",
        "times" => "times",
        "cdot" => "dot",
        "div" => "div",
        "pm" => "plus.minus",
        "mp" => "minus.plus",
        "ast" => "ast",
        "star" => "star",
        "circ" => "compose",
        "bullet" => "bullet",
        "oplus" => "plus.circle",
        "otimes" => "times.circle",
        "le" | "leq" => "lt.eq",
        "ge" | "geq" => "gt.eq",
        "ll" => "lt.double",
        "gg" => "gt.double",
        "ne" | "neq" => "eq.not",
        "gtrsim" => "gt.tilde",
        "lesssim" => "lt.tilde",
        "gtrless" => "gt.lt",
        "lessgtr" => "lt.gt",
        "nless" => "lt.not",
        "ngtr" => "gt.not",
        "nleq" => "lt.eq.not",
        "ngeq" => "gt.eq.not",
        "approx" => "approx",
        "equiv" => "equiv",
        "sim" => "tilde.op",
        "propto" => "prop",
        "to" | "rightarrow" => "arrow.r",
        "leftarrow" | "gets" => "arrow.l",
        "leftrightarrow" => "arrow.l.r",
        "Rightarrow" | "implies" => "arrow.r.double",
        "Leftarrow" => "arrow.l.double",
        "Leftrightarrow" | "iff" => "arrow.l.r.double",
        "mapsto" => "arrow.r.bar",
        "uparrow" => "arrow.t",
        "downarrow" => "arrow.b",
        "in" => "in",
        "notin" => "in.not",
        "ni" => "in.rev",
        "subset" => "subset",
        "supset" => "supset",
        "subseteq" => "subset.eq",
        "supseteq" => "supset.eq",
        "cup" => "union",
        "cap" => "sect",
        "setminus" => "without",
        "emptyset" | "varnothing" => "emptyset",
        "sum" => "sum",
        "prod" => "product",
        "coprod" => "coproduct",
        "int" => "integral",
        "iint" => "integral.double",
        "iiint" => "integral.triple",
        "oint" => "integral.cont",
        "bigcup" => "union.big",
        "bigcap" => "sect.big",
        "bigoplus" => "plus.circle.big",
        "bigotimes" => "times.circle.big",
        "bigvee" => "or.big",
        "bigwedge" => "and.big",
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "sec" => "sec",
        "csc" => "csc",
        "arcsin" => "arcsin",
        "arccos" => "arccos",
        "arctan" => "arctan",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        "exp" => "exp",
        "log" => "log",
        "ln" => "ln",
        "lg" => "lg",
        "lim" => "lim",
        "limsup" => "limsup",
        "liminf" => "liminf",
        "sup" => "sup",
        "inf" => "inf",
        "max" => "max",
        "min" => "min",
        "arg" => "arg",
        "det" => "det",
        "dim" => "dim",
        "gcd" => "gcd",
        "deg" => "deg",
        "mod" => "mod",
        "bmod" => "mod",
        "infty" => "infinity",
        "partial" => "partial",
        "nabla" => "nabla",
        "forall" => "forall",
        "exists" => "exists",
        "neg" | "lnot" => "not",
        "land" | "wedge" => "and",
        "lor" | "vee" => "or",
        "ell" => "ell",
        "hbar" => "planck.reduce",
        "Re" => "Re",
        "Im" => "Im",
        "aleph" => "aleph",
        "prime" => "prime",
        "degree" => "degree",
        "angle" => "angle",
        "top" => "top",
        "bot" => "bot",
        "langle" => "chevron.l",
        "rangle" => "chevron.r",
        "lfloor" => "floor.l",
        "rfloor" => "floor.r",
        "lceil" => "ceil.l",
        "rceil" => "ceil.r",
        "lbrace" => "brace.l",
        "rbrace" => "brace.r",
        "vert" | "lvert" | "rvert" | "mid" => "bar.v",
        "Vert" | "lVert" | "rVert" => "bar.v.double",
        "backslash" => "backslash",
        "perp" => "perp",
        "parallel" => "parallel",
        "cong" => "tilde.equiv",
        "ldots" | "dots" => "dots",
        "cdots" => "dots.c",
        "vdots" => "dots.v",
        "ddots" => "dots.down",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn room() -> Room {
        Room {
            cell: (10, 20),
            cols: 100,
            rows: 30,
            colour: (205, 214, 244),
            scale: 1.0,
        }
    }

    #[test]
    fn a_formula_is_rewritten_into_typst_s_own_notation() {
        assert_eq!(latex(r"\frac{a+b}{2}").unwrap(), "frac(a+b, 2)");
        assert_eq!(latex(r"\sqrt{x}").unwrap(), "sqrt(x)");
        assert_eq!(latex(r"\sqrt[3]{x+1}").unwrap(), "root(3, x+1)");
        assert_eq!(latex(r"\alpha \cdot \beta").unwrap(), "alpha dot beta");
        assert_eq!(
            latex(r"k \epsilon^2 \gtrsim \log n").unwrap(),
            "k epsilon^2 gt.tilde log n"
        );
        assert_eq!(
            latex(r"a \lesssim b \gtrless c \lessgtr d").unwrap(),
            "a lt.tilde b gt.lt c lt.gt d"
        );
        assert_eq!(
            latex(r"x \nless y \nleq z \ngeq w \ngtr v").unwrap(),
            "x lt.not y lt.eq.not z gt.eq.not w gt.not v"
        );
        assert_eq!(latex(r"a \gtrapprox b"), None);
        assert_eq!(latex(r"x^2 + y_i").unwrap(), "x^2 + y_i");
        assert_eq!(latex(r"\sin^2 x").unwrap(), "sin^2 x");
        assert_eq!(
            latex(r"\int_0^1 f(x)\,dx").unwrap(),
            "integral_0^1 f(x) d x"
        );
        assert_eq!(latex(r"\mathbb{R}").unwrap(), "bb(R)");
        assert_eq!(latex(r"\hat{x}").unwrap(), "hat(x)");
        assert_eq!(latex(r"r^\top h").unwrap(), "r^top h");
        assert_eq!(latex(r"a \bot b").unwrap(), "a bot b");
        assert_eq!(latex(r"\text{if } x>0").unwrap(), "\"if \" x>0");
        assert_eq!(
            latex(r"a \\ b").unwrap(),
            "a \\ b",
            "a line break stays one"
        );
        assert_eq!(
            latex(r"\frac{\alpha+\beta}{2}").unwrap(),
            "frac(alpha+beta, 2)"
        );
    }

    #[test]
    fn an_environment_becomes_the_call_that_draws_it() {
        assert_eq!(
            latex(r"\begin{pmatrix}a & b \\ c & d\end{pmatrix}").unwrap(),
            "mat(delim: \"(\", a, b; c, d)"
        );
        assert_eq!(
            latex(r"\begin{bmatrix}1 \\ 2\end{bmatrix}").unwrap(),
            "mat(delim: \"[\", 1; 2)"
        );
        assert_eq!(
            latex(r"\begin{cases}x & x>0 \\ 0 & \text{otherwise}\end{cases}").unwrap(),
            "cases(x & x>0, 0 & \"otherwise\")"
        );
        assert_eq!(
            latex(r"\begin{aligned}a &= b \\ c &= d\end{aligned}").unwrap(),
            "a &= b \\ c &= d"
        );
    }

    #[test]
    fn an_environment_is_refused_when_one_of_its_cells_cannot_be_translated() {
        assert_eq!(
            latex(r"\begin{pmatrix}a & b \\ \foo & d\end{pmatrix}"),
            None
        );
        assert_eq!(latex(r"ka\ll 1"), Some("k a lt.double 1".into()));
        assert_eq!(latex(r"x\gg y"), Some("x gt.double y".into()));
    }

    #[test]
    fn a_command_this_file_does_not_know_is_refused_rather_than_guessed() {
        assert_eq!(latex(r"\foo{x}"), None);
        assert_eq!(latex(r"\begin{tikzpicture}\end{tikzpicture}"), None);
        assert_eq!(latex(r"a \over b"), None, "a TeX primitive, not a command");
        assert_eq!(
            latex(r"\frac{a}{\frac{b}{c}}").unwrap(),
            "frac(a, frac(b, c))"
        );
    }

    #[test]
    fn a_formula_is_typeset_into_a_picture_at_the_terminal_s_pixel_size() {
        let room = Room {
            cell: (8, 17),
            cols: 100,
            rows: 10,
            colour: (220, 220, 220),
            scale: 1.0,
        };
        let formula = render(r"E = m c^2", &room).expect("a formula renders");
        assert_eq!(formula.image.media_type, "image/png");
        assert!(formula.image.width > 8, "{:?}", formula.image);
        assert_eq!(formula.cells.1, 2, "{formula:?}");
        assert!(
            formula.image.height >= 2 * room.cell.1,
            "the raster is finer than the box it fills: {:?}",
            formula.image
        );
        let again = render(r"E = m c^2", &room).expect("a formula renders");
        assert_eq!(formula.image.data, again.image.data);
    }

    #[test]
    fn a_formula_in_a_line_is_one_row_of_cells() {
        let room = Room {
            cell: (8, 17),
            cols: 100,
            rows: 1,
            colour: (220, 220, 220),
            scale: 1.0,
        };
        let formula = render_inline(r"a^2 + b^2", &room).expect("a formula renders");
        let bound = render_inline(r"k \epsilon^2 \gtrsim \log n", &room)
            .expect("an inline formula renders");
        assert_eq!(bound.cells.1, 1, "one row: {bound:?}");
        assert!(formula.inline, "it sits inside a line");
        assert_eq!(
            formula.cells.1, 1,
            "one row, whatever it holds: {formula:?}"
        );
        assert!(formula.cells.0 > 2, "as wide as it is: {formula:?}");
        assert!(
            formula.image.height >= room.cell.1 * RESOLUTION,
            "the raster is finer than the box it fills: {:?}",
            formula.image
        );
        let drawn = formula.image.width as f32 / formula.image.height as f32;
        let box_ratio = (formula.cells.0 as f32 * room.cell.0 as f32) / room.cell.1 as f32;
        assert!(
            (drawn / box_ratio - 1.0).abs() < 0.02,
            "the picture is the box's shape: {drawn} against {box_ratio}, {:?}",
            formula.image
        );
        let again = render_inline(r"a^2 + b^2", &room).expect("a formula renders");
        assert_eq!(formula.image.data, again.image.data);
    }

    #[test]
    fn a_formula_in_a_line_sits_on_the_line_s_baseline() {
        let room = Room {
            cell: (8, 20),
            cols: 100,
            rows: 1,
            colour: (0, 0, 0),
            scale: 1.0,
        };
        let formula = render_inline("x", &room).expect("a letter renders");
        let rows = ink_rows_of(&formula.image.data);
        let (top, bottom) = rows.expect("a letter has ink");
        let height = formula.image.height as f32;
        let baseline = INLINE_BASELINE * height;
        assert!(
            (bottom as f32 - baseline).abs() <= 2.0,
            "the ink ends on the baseline: ink {top}..{bottom} of {height}, baseline {baseline}"
        );
        assert!((top as f32) < baseline, "above the baseline");
        assert!(top as f32 > height * 0.2, "not at the very top: {top}");
    }

    #[test]
    fn a_formula_in_a_line_too_tall_for_one_row_is_left_to_its_source() {
        let room = Room {
            cell: (8, 20),
            cols: 100,
            rows: 1,
            colour: (0, 0, 0),
            scale: 1.0,
        };
        assert!(render_inline(r"\frac{1}{1 + \frac{1}{x}}", &room).is_none());
        assert!(render_inline(r"\frac{a}{b}", &room).is_some());
    }

    fn ink_rows_of(png: &str) -> Option<(u32, u32)> {
        let bytes = general_purpose::STANDARD.decode(png).ok()?;
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().ok()?;
        let mut buffer = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buffer).ok()?;
        let row_bytes = info.width as usize * 4;
        let mut first = None;
        let mut last = 0;
        for y in 0..info.height as usize {
            let row = &buffer[y * row_bytes..(y + 1) * row_bytes];
            if row.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 8) {
                first.get_or_insert(y as u32);
                last = y as u32;
            }
        }
        first.map(|first| (first, last))
    }

    #[test]
    fn a_larger_scale_draws_a_larger_formula() {
        let one = render(r"E = m c^2", &room()).expect("a formula renders");
        let bigger = Room {
            scale: 1.5,
            ..room()
        };
        let and_a_half = render(r"E = m c^2", &bigger).expect("a formula renders");
        assert!(
            and_a_half.cells.0 >= one.cells.0 && and_a_half.cells.1 >= one.cells.1,
            "{and_a_half:?} {one:?}"
        );
        assert!(
            and_a_half.image.width > one.image.width,
            "{:?} {:?}",
            and_a_half.image,
            one.image
        );
        assert_ne!(and_a_half.image.data, one.image.data);
        let absurd = Room {
            scale: 500.0,
            ..room()
        };
        let capped = render(r"E = m c^2", &absurd).expect("a formula renders");
        let four = Room {
            scale: MAX_SCALE,
            ..room()
        };
        let allowed = render(r"E = m c^2", &four).expect("a formula renders");
        assert_eq!(
            (capped.cells, capped.image.width, capped.image.height),
            (allowed.cells, allowed.image.width, allowed.image.height)
        );
    }

    #[test]
    fn a_formula_with_no_room_is_left_to_its_source() {
        assert!(render(r"\frac{a+b+c+d+e+f}{g+h+i+j+k}", &Room::tiny()).is_none());
        assert!(render(r"\foo{x}", &Room::tiny()).is_none());
    }

    #[test]
    fn a_letter_run_in_latex_is_letters_and_the_typst_name_is_a_command() {
        assert_eq!(latex("x^2 dx"), Some("x^2 d x".into()));
        assert_eq!(latex("alpha"), Some("a l p h a".into()));
        assert_eq!(latex(r"\alpha"), Some("alpha".into()));
        assert_eq!(latex("frac(a, b)"), Some("f r a c(a, b)".into()));
        assert!(render_typst("frac(a, b)", &room()).is_some());
        let latex = render(r"\frac{a}{b}", &room()).expect("the LaTeX fraction");
        let typst = render_typst("frac(a, b)", &room()).expect("the Typst fraction");
        assert_eq!(
            latex.cells, typst.cells,
            "the two invocations reach the same picture"
        );
    }

    #[test]
    fn a_formula_written_the_way_people_write_it_is_typeset() {
        for formula in [
            r"\int_0^1 x^2 \, dx = \frac{1}{3}",
            r"\sum_{k=1}^{n} k = \frac{n(n+1)}{2}",
            r"\det \begin{pmatrix} a & b \\ c & d \end{pmatrix} = ad - bc",
            r"\sqrt[3]{\frac{\alpha + \beta}{2}} \le \lim_{n\to\infty} \left(1 + \frac{1}{n}\right)^n",
            r"\frac{\partial^2 u}{\partial t^2} = c^2 \nabla^2 u + \frac{1}{\rho} \int_{\Omega} f \, dV",
            r"\{ x \in \mathbb{R} : x > 0 \}",
            r"r^\top \delta > b-r^\top h_q",
            r"k \epsilon^2 \gtrsim \log n",
        ] {
            assert!(
                render(formula, &room()).is_some(),
                "{formula} -> {:?}",
                latex(formula)
            );
        }
    }

    #[test]
    fn two_words_do_not_run_into_one() {
        assert_eq!(latex(r"n\to\infty"), Some("n arrow.r infinity".into()));
        assert_eq!(latex(r"\alpha x"), Some("alpha x".into()));
        assert_eq!(latex(r"\alpha\beta"), Some("alpha beta".into()));
        assert_eq!(latex("ab"), Some("a b".into()));
        assert_eq!(latex(r"\left( x \right)^n"), Some("( x )^n".into()));
        assert_eq!(latex(r"\left\{ x \right."), Some("brace.l x".into()));
    }
}
