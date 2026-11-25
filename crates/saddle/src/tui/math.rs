//! Mathematics in the transcript (D165).
//!
//! A formula is typeset by Typst — New Computer Modern, and TeX's rules for fractions, radicals,
//! growing delimiters and big operators — and the result is placed as a picture at **one image
//! pixel per screen pixel**, which the terminal's graphics protocol draws exactly as typeset. The
//! two alternatives were considered and rejected in the decision: Unicode glyphs on the character
//! grid cannot scale a glyph or move it by a fraction of a cell, so `\frac` and `\sqrt` come out as
//! `a/b` and `√(x)`; brake dots are 2×4 per cell, a quarter of the screen's pixels, and a formula
//! drawn that way is a picture of a picture.
//!
//! Both `$…$` spellings arrive already parsed from the markdown parser (`Options::ENABLE_MATH`).
//! Their content is **LaTeX**, and only LaTeX: `\frac{a}{b}`, `\sqrt[3]{x}`,
//! `\begin{pmatrix}…\end{pmatrix}` and `\alpha` are translated into Typst's math syntax by
//! [`latex`], and a letter run is letters, as TeX sets it. Typst's own notation is a different
//! invocation — a fence tagged `typst-math` — so no body is ever read as both languages and read
//! the wrong way. A formula this front-end cannot translate is drawn as its source, never as
//! something wrong.
//!
//! The size is derived from the terminal's own cell (D59): a line of maths is a shade larger than a
//! line of text, so one image pixel is one screen pixel and the terminal never scales the picture.
//! That is what makes the reserved rows and the drawn formula agree by construction.

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
use typst::layout::PagedDocument;
use typst::syntax::FileId;
use typst::syntax::Source;
use typst::syntax::VirtualPath;
use typst::text::Font;
use typst::text::FontBook;
use typst::utils::LazyHash;
use typst_render::render as rasterise;

use crate::provider::Image;

/// The font size a formula is laid out at, before it is rasterised at the terminal's pixel scale.
const FONT_PT: f32 = 11.0;

/// A line of text at [`FONT_PT`]: ascent, descent and leading, in points. Used to put a formula at
/// the same size as the prose around it.
const LINE_PT: f32 = 13.2;

/// How much larger a display formula is drawn than the text: the usual TeX `\displaystyle` jump,
/// and what makes an integral or a sum legible at all.
const DISPLAY: f32 = 1.15;

/// What the two fonts disagree about even at the same size.
///
/// A terminal's font has a lowercase letter about a fifth taller than New Computer Modern's at the
/// same em — monospace faces are drawn to a large x-height so that `l` and `I` can be told apart —
/// so a formula scaled to the text's em is read as smaller than the words beside it. This is the
/// ratio that makes the two match: what a formula is sized against is its lowercase letters, not
/// its em. `[ui] math-scale` divides it further, for whoever wants it larger still.
const X_HEIGHT: f32 = 1.2;

/// The smallest a formula may be shrunk to fit the room before its source is drawn instead. Below
/// this it is not readable, and a formula nobody can read is worse than the TeX it came from.
const FLOOR: f32 = 0.55;

/// The room left inside the picture around the formula: a cell of the transcript is taller than the
/// letters in it, and a formula whose last row touches the text below reads as crowded. Horizontal
/// room keeps a bracket from touching the word beside it. The top gets next to none: a formula's own
/// line box already leaves the room its ascenders need above them, so padding there is added to a gap
/// that is there — measured on a real terminal, 3pt above read as a hole under the previous line.
/// The bottom is where the room is really wanted, because a formula's descenders — the denominator of
/// a fraction, the tail of an integral — hang into it, and because a formula is a block that the text
/// after it has to be separated from (D165).
const PAD_X_PT: f32 = 2.0;
const PAD_TOP_PT: f32 = 1.0;
const PAD_BOTTOM_PT: f32 = 7.0;

/// How many image pixels one screen pixel of a formula is rasterised at.
///
/// The picture is scaled into its cells whatever its pixel count, so more pixels cost only bytes
/// and buy sharpness. The cell saddle measured may be a guess on a display that never answered, so
/// the raster is doubled: that is headroom for a display twice as fine as the guess, and a terminal
/// finer still shrinks what it was given rather than enlarging it.
const RESOLUTION: u32 = 2;

/// The most pixels one formula may be rasterised at, so a long formula on a wide terminal cannot
/// become megabytes of base64 for sharpness nobody can see.
const MAX_PIXELS: f64 = 4_000_000.0;

/// What `[ui] math-scale` is held to: a tenth of the formula's own size would be illegible and ten
/// times it would be one glyph on the whole screen, and a mistyped `math-scale` is neither.
const MIN_SCALE: f32 = 0.5;
const MAX_SCALE: f32 = 4.0;

/// Where a terminal's font puts the baseline inside a cell, as a fraction of the cell's height.
///
/// An inline formula is one row tall, so its own baseline has to land on the text's, and no
/// terminal answers a question about where that is. It is an assumption of the same kind as the
/// cell itself (D59): the cell a font sits in carries its descent below the baseline and its
/// ascender room above, in roughly a quarter and three quarters of the row. Being wrong by a pixel
/// is invisible, and this is the number to move if a formula reads high or low beside its line.
const INLINE_BASELINE: f32 = 0.75;

/// The room an inline formula keeps beside the words it sits between, inside its own picture: a
/// bracket touching the word beside it reads as part of that word.
const INLINE_PAD_X_PT: f32 = 1.5;

/// The room above and below its baseline an inline formula is given, in points, before it is refused
/// as too tall for one row.
///
/// It is spent as an invisible strut in the same line as the formula, which is what makes the
/// baseline's place in the picture known instead of guessed: a page that comes out exactly twice
/// this tall has that much above the baseline and that much below it, wherever the formula's own
/// ink happens to sit. A formula taller than the strut makes the page taller, which is how the
/// refusal is detected (D165).
const INLINE_STRUT_PT: f32 = 32.0;

/// What a formula is drawn with, and into what room.
pub struct Room {
    /// One cell, in pixels (D59).
    pub cell: (u32, u32),
    /// The transcript's width in cells, and the most rows a formula may have (D59).
    pub cols: u16,
    pub rows: u16,
    /// The colour the formula is drawn in: the theme's own, so it sits in the palette.
    pub colour: (u8, u8, u8),
    /// `[ui] math-scale` against the size the fonts and the cell work out to: 1.0 is the default,
    /// and a larger number is a larger formula, up to the room.
    pub scale: f32,
}

impl Room {
    /// A room that is never available in practice, for a test that wants the error paths.
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

/// What a formula was written in, which is how it is read and how a copy of it is spelled (D166).
///
/// The two are separate notations and never the same text read twice: `$…$` and `$$…$$` are LaTeX
/// and only LaTeX, while Typst's own maths arrives in a fence that says so. A body that read as
/// either would be a body that can be read the wrong way, silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notation {
    /// `$…$` in a sentence, `$$…$$` on its own line, `\(…\)` and `\[…\]`: LaTeX, translated by
    /// [`latex`].
    Latex,
    /// A fence tagged [`TYPST`], whose body is Typst's own maths and is handed to the typesetter as
    /// written.
    Typst,
}

/// The fence tag that asks for a formula written the Typst way (D165).
///
/// Not `typst`, which is how a block of Typst *code* is tagged: the tag has to say maths, or the
/// same block would be a formula when it happens to parse and code when it does not.
pub const TYPST: &str = "typst-math";

/// A typeset formula: the picture, and the cells of the transcript it is drawn in.
///
/// The two are separate on purpose (D165). The cells are the size — the rows the layout reserves and
/// the box the terminal is asked to fill — while the picture's own pixels are only a matter of how
/// sharp it is, so they are oversampled rather than tied to a cell saddle may have had to guess.
#[derive(Clone, Debug)]
pub struct Formula {
    pub image: Image,
    pub cells: (u16, u16),
    /// Whether the formula is drawn inside a line of text rather than as a block of its own: it is
    /// then one row tall, and it is placed on its row at a column instead of at the row's start.
    pub inline: bool,
    /// What the source was written in, which decides the delimiters a copy of it gets (D166).
    pub notation: Notation,
    /// The formula as it was written, which is what a copy of the picture takes: the picture is what
    /// is on screen, but the words are what goes in a paper (D166).
    ///
    /// It is not part of the picture's key: two spellings that typeset alike share a raster, and
    /// each copy still gets the spelling it came from.
    pub source: String,
}

/// Typesets one formula written in LaTeX — `$…$`, `$$…$$`, `\(…\)`, `\[…\]` — into a picture the
/// transcript can place, or `None` when it cannot be typeset, in which case the caller draws the
/// source (D165).
pub fn render(source: &str, room: &Room) -> Option<Formula> {
    let body = latex(source)?;
    place(&body, source, Notation::Latex, room, false)
}

/// Typesets one formula written the Typst way, which is what a fence tagged [`TYPST`] asks for
/// (D165).
///
/// The body goes to the typesetter as written: there is nothing to translate, and no second reading
/// of a body that LaTeX would also have accepted.
pub fn render_typst(source: &str, room: &Room) -> Option<Formula> {
    place(source.trim(), source, Notation::Typst, room, false)
}

/// One picture, drawn from a body and remembering where its words came from (D165, D166).
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
        // The picture is shared between spellings; the words are not (D166).
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
        // A transcript re-renders a message on every width change; a formula is a few kilobytes and
        // the map is bounded by the number of distinct formulas the session has shown.
        cache.insert(key, formula.clone());
    }
    Some(formula)
}

/// The rendered formulas this process has already typeset, keyed by everything that decides the
/// picture (D165).
fn cache() -> &'static Mutex<HashMap<Key, Formula>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Formula>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Everything that decides the picture: the translated formula, the cell, the room, the colour and
/// the person's own scale.
type Key = (String, (u32, u32), u16, u16, (u8, u8, u8), u32, bool);

/// The file the snippet is compiled as.
const MAIN: &str = "/equation.typ";

/// Typesets Typst math and rasterises it at the terminal's own pixel scale (D165).
fn typeset(body: &str, room: &Room) -> Option<Formula> {
    let document = compile(body, room, false)?;
    let page = document.pages.first()?;
    let size = page.frame.size();
    let (width_pt, height_pt) = (size.x.to_pt(), size.y.to_pt());
    if width_pt <= 0.0 || height_pt <= 0.0 {
        return None;
    }
    let (cell_w, cell_h) = (room.cell.0.max(1) as f64, room.cell.1.max(1) as f64);
    // The size is in cells, because that is what the transcript is laid out in and what the
    // terminal fills: a display formula is set a shade larger than the text, and one line's worth of
    // it is one row of the transcript. Shrinking to the room is the only thing that changes it.
    let chosen = DISPLAY as f64 * X_HEIGHT as f64 * room.scale.clamp(MIN_SCALE, MAX_SCALE) as f64;
    let natural = cell_h / LINE_PT as f64;
    let mut scale = chosen * natural;
    scale = scale.min(room.cols.max(1) as f64 * cell_w / width_pt);
    scale = scale.min(room.rows.max(1) as f64 * cell_h / height_pt);
    if scale < natural * FLOOR as f64 {
        return None;
    }
    // The cells that box comes to, and then the pixels: as many as the box would have on a display
    // finer than the cell saddle measured, so the terminal is unlikely to be enlarging anything it
    // was given.
    let cells = (
        ((width_pt * scale / cell_w).round() as u16).max(1),
        ((height_pt * scale / cell_h).round() as u16).max(1),
    );
    let refine = (scale * RESOLUTION as f64).min((MAX_PIXELS / (width_pt * height_pt)).sqrt());
    let pixmap = rasterise(page, refine as f32);
    let (width, height) = (pixmap.width(), pixmap.height());
    if width == 0 || height == 0 {
        return None;
    }
    let bytes = encode_png(pixmap.width(), pixmap.height(), pixmap.data())?;
    Some(Formula {
        cells,
        inline: false,
        // Filled in by `place`, which is the one thing that knows where the words came from (D166).
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

/// Typesets one formula for a line of text: one row tall, sitting on the line's baseline (D165).
///
/// A display formula is a block, and its rows are the room the transcript gives it. Inline there is
/// no such room: the cell is the whole height, the words on either side are set on its baseline, and
/// the formula has to meet them there. So the picture is built to be exactly one row, with the
/// formula's own baseline [`INLINE_BASELINE`] of the way down it, and it is shrunk to get there —
/// refused instead when shrinking would leave it unreadable.
pub fn render_inline(source: &str, room: &Room) -> Option<Formula> {
    let body = latex(source)?;
    place(&body, source, Notation::Latex, room, true)
}

/// The inline half of [`typeset`]: the picture is a crop of a page whose baseline is known.
///
/// The page carries the formula in one line with an invisible strut above and below the baseline, so
/// a page exactly two struts tall is a page whose baseline is exactly one strut down from its top —
/// whatever the formula's ink does. The ink is then found in the raster, so the crop can be placed
/// around it: room above it for the ascenders of the words beside it, room below for its own
/// descenders, and the baseline where the terminal's font puts the text's (D165).
fn typeset_inline(body: &str, room: &Room) -> Option<Formula> {
    let document = compile(body, room, true)?;
    let page = document.pages.first()?;
    let size = page.frame.size();
    let (width_pt, height_pt) = (size.x.to_pt() as f32, size.y.to_pt() as f32);
    if width_pt <= 0.0 {
        return None;
    }
    // The strut decides where the baseline is only while it is the tallest thing in the line. A
    // taller page means the formula overhung it, which is a formula that cannot be one row.
    if (height_pt - 2.0 * INLINE_STRUT_PT).abs() > 0.01 {
        return None;
    }
    let (cell_w, cell_h) = (room.cell.0.max(1) as f32, room.cell.1.max(1) as f32);
    // A formula is set at the size the prose beside it is matched at (below), so that size is the
    // density the raster is taken at, whatever the formula turns out to need: a formula shrunk to
    // fit the row arrives with more pixels than it is drawn at, which is sharpness rather than size.
    let natural = LINE_PT / X_HEIGHT;
    let probe = cell_h / natural * RESOLUTION as f32;
    let pixmap = rasterise(page, probe);
    let (page_px, page_rows) = (pixmap.width(), pixmap.height());
    if page_px == 0 || page_rows == 0 {
        return None;
    }
    let baseline_px = INLINE_STRUT_PT * probe;
    let (ink_top, ink_bottom) = ink_rows(pixmap.data(), page_px, page_rows)?;
    let ascent = (baseline_px - ink_top as f32) / probe;
    let descent = (ink_bottom as f32 + 1.0 - baseline_px) / probe;
    // The smallest box that holds the ink on both sides of the baseline, and no smaller than the
    // size the prose beside it is matched at: what is matched is the lowercase letters, not the em
    // (see `X_HEIGHT`), so a formula in a line is set against a line's worth of points divided by
    // that correction rather than against the line itself. A formula whose ink needs more than that
    // is drawn smaller, which is what TeX's text style does with a fraction too.
    let below = 1.0 - INLINE_BASELINE;
    let need = (ascent / INLINE_BASELINE).max(descent / below);
    let height_box = need.max(natural) * room.scale.clamp(MIN_SCALE, MAX_SCALE);
    // Shrinking is what makes a formula one row tall; past the floor it is not read at all, and the
    // source is the better thing to draw.
    if natural / height_box < FLOOR {
        return None;
    }
    // The cells the row gives it: as many as the picture's width comes to once it is drawn at the
    // size its height allows.
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
    // The picture is given exactly the box's proportions, with transparent columns either side of
    // it: a terminal fits an image into the cells it is asked for, and one whose shape is not the
    // box's would be shrunk on one axis — leaving it short of the row the baseline is measured
    // from, which is the one thing an inline formula cannot afford (D165).
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

/// The first and last row of a raster that has ink in it, or `None` when it has none at all.
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

/// `rows` rows of a raster from `top` down, centred in `width` columns of transparency.
///
/// The widening is the difference between the formula's own width and the whole cells the row gives
/// it, split evenly so that neither word beside it is the one that gets the gap (D165).
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

/// One compile of one formula, `inline` or as a block of its own (D165).
///
/// One reading, and only one: the body is what the notation said it was, so a refusal is the
/// answer — the caller draws the source rather than trying the same text as the other language.
fn compile(body: &str, room: &Room, inline: bool) -> Option<PagedDocument> {
    layout(body, room, inline)
}

///
/// The two spellings of a formula are also two modes of Typst's own: `$ … $` with the spaces set
/// aside from the delimiters is display maths, which is a block set large with its limits above and
/// below the operator, and `$…$` is what a formula inside a sentence is. An inline formula is set in
/// the second mode, which is half of why it fits in a row.
fn layout(body: &str, room: &Room, inline: bool) -> Option<PagedDocument> {
    let (r, g, b) = room.colour;
    let text = if inline {
        // The strut is invisible and has no width: it only says that the line it is in has
        // `INLINE_STRUT_PT` above the baseline and the same below, so the page's own height tells
        // the raster where the baseline is (D165).
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
    let world = Snippet::new(&text);
    typst::compile(&world).output.ok()
}

/// The pixmap as a PNG, with the premultiplied alpha a rasteriser produces undone: the terminal
/// composites the picture over its own background, so an edge pixel must carry its own colour.
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

/// One compilation: the snippet, and the fonts that came with the binary.
struct Snippet {
    main: FileId,
    source: Source,
    library: LazyHash<Library>,
    book: &'static LazyHash<FontBook>,
    fonts: &'static [Font],
}

impl Snippet {
    fn new(text: &str) -> Self {
        let (book, fonts) = fonts();
        // The path is a name for the error messages, not a file: nothing here is read from disk.
        let main = FileId::new(None, VirtualPath::new(MAIN));
        Self {
            main,
            source: Source::new(main, text.to_owned()),
            library: LazyHash::new(Library::default()),
            book,
            fonts,
        }
    }
}

/// The bundled fonts, loaded once: Typst's font book is what costs a formula's first rendering, and
/// every formula after it reuses the same one (D165).
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
        // Nothing is read from the machine the renderer runs on: a formula is a string and a font.
        Err(FileError::NotFound(MAIN.into()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        // A formula has no idea what day it is, and nothing in one asks.
        None
    }
}

/// A body being translated, remembering whether the last thing written was a name or a single
/// character.
///
/// The two must not run together: `n`, then `\to` is `narrow.r`, and `\alpha`, then `x` is
/// `alphax` — both names that mean something else or nothing at all. Two characters are already
/// separate variables (`ab` is `a` times `b`), and a space in math is not drawn, so the separator
/// costs nothing when it is not needed.
#[derive(Default)]
struct Emitted {
    text: String,
    name: bool,
}

impl Emitted {
    /// One name, call or symbol: `alpha`, `frac(1, 2)`, `arrow.r`.
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

    /// One character of the formula: a letter, a digit, a bracket, an operator.
    fn chr(&mut self, c: char) {
        if self.name && c.is_ascii_alphanumeric() {
            self.text.push(' ');
        }
        self.text.push(c);
        self.name = false;
    }
}

/// Appends a token, deciding from its shape whether it is a name or a character.
fn emit(out: &mut Emitted, text: &str) {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => out.chr(c),
        _ => out.name(text),
    }
}

/// Translates one LaTeX formula into Typst's maths, which is the only reading a `$…$` body gets
/// (D165). `None` means "this front-end does not know that", and the caller draws the source.
///
/// The two notations agree about the characters — `x^2`, `_i`, `+`, `(`, and the letters themselves
/// — so this is a rewrite of the commands rather than a second parser: `\frac{a}{b}` becomes
/// `frac(a, b)`, `\sqrt[3]{x}` becomes `root(3, x)`, `\alpha` becomes `alpha`, and an environment
/// becomes the Typst call that draws it. What it is not is a guess: a body written the Typst way is
/// read as LaTeX, because LaTeX is what the delimiters mean — `frac(a, b)` is six letters there —
/// and Typst's own notation has an invocation of its own ([`TYPST`]).
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
                    // `\\` is a line break, `\,` a thin space, `\{` a literal brace.
                    let Some(next) = chars.get(i) else {
                        break;
                    };
                    i += 1;
                    // `{`, `%`, `#` and `$` mean something to Typst's parser, so the escaped
                    // character is written as the symbol that draws it.
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
                // A group is a Typst parenthesised group: it is only ever grouping.
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
            // A letter run is letters: LaTeX has no multi-letter variable, so `dx` is `d` times
            // `x`, and Typst would read `dx` as a name it does not have and refuse the whole
            // formula. A space in maths is not drawn, so writing the letters apart is how TeX sets
            // the same run (D165). It is also what makes this LaTeX and not the other notation: in
            // Typst `alpha` is a name, here it is five letters, and the way to write the name is
            // `\alpha`.
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
            // Everything else means the same thing in both notations.
            _ => {
                emit(&mut out, &c.to_string());
                i += 1;
            }
        }
    }
    Some(out.text.trim().to_owned())
}

/// One LaTeX control word: a symbol, a function of its arguments, or nothing this file knows.
fn command(name: &str, chars: &[char], i: &mut usize, out: &mut Emitted) -> bool {
    // A symbol, either a Typst name or the Unicode character it draws.
    if let Some(symbol) = symbol(name) {
        emit(out, symbol);
        return true;
    }
    // Functions of one or two arguments, given as `{…}` or `(…)`.
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
            // `\sqrt[3]{x}` is a root, `\sqrt{x}` the square one.
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
            // Verbatim, spaces and all: this argument is not maths (D165).
            let Some(text) = literal(chars, i) else {
                return false;
            };
            // Typst has no command for these: a quoted string is text in math, and `op` makes a
            // multi-letter name stand upright without the spacing of separate variables.
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
        // Typst grows a delimiter by itself, so `\left` and `\right` only say which one it is:
        // `\left.` says none, and `\left\{` is the brace symbol rather than Typst's grouping.
        "left" | "right" => {
            let Some(delimiter) = delimiter(chars, i) else {
                return false;
            };
            emit(out, &delimiter);
            true
        }
        "displaystyle" | "textstyle" | "scriptstyle" | "limits" | "nolimits" => {
            // Typst chooses the style itself: a display formula is display, limits belong over and
            // under their operator.
            true
        }
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

/// A LaTeX environment: the ones Typst has an equivalent for, and nothing else.
fn environment(chars: &[char], i: &mut usize, out: &mut Emitted) -> bool {
    // The name is read as it is written, not as maths: `matrix` is one word naming an environment,
    // and a letter run put through [`latex`] is `m a t r i x` (D165).
    let Some(name) = literal(chars, i) else {
        return false;
    };
    let name = name.trim();
    // The body runs to the matching `\end{…}`.
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
    // Rows are separated by `\\` and columns by `&` in both notations. A cell this front end
    // cannot translate refuses the whole environment: dropping it would draw a matrix with holes in
    // it, and the rule is that what cannot be drawn is drawn as its source, never as something
    // wrong (D165).
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
            // Typst's `cases` takes one array per row — cells separated by `&`, which is what
            // aligns them — so it is written differently from a matrix's `;`.
            let rows: Vec<String> = cells.iter().map(|row| row.join(" & ")).collect();
            out.name(&format!("cases({})", rows.join(", ")))
        }
        "aligned" | "align" | "align*" | "gathered" | "split" => {
            // Alignment and line breaks mean the same thing, so the body is passed through with
            // its `&`s and breaks intact.
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

/// One argument taken as it is written, for a command whose argument is text rather than maths
/// (D165): `\text{if }` keeps its space.
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

/// The delimiter a `\left` or `\right` names, in Typst's spelling — empty when it names none,
/// as `\left.` does.
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

/// One argument: `{…}`, `(…)`, a control word, or a single character.
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
            // The character may be a complete Typst atom, or the start of one.
            Some(c.to_string())
        }
    }
}

/// A quoted string, with the characters Typst's strings treat specially escaped.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// LaTeX's symbols and the Typst name or character that means the same thing.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek, lower and upper.
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
        // Operators and relations.
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
        // The slanted and negated inequalities: `\gtrsim` in a rate bound is Typst's `gt.tilde`,
        // and the crossed pair is `lt.gt`/`gt.lt`. Typst has no ≳-or-≈ or ≲-or-≈ glyph, so
        // `\gtrapprox` and `\lessapprox` stay unknown rather than being drawn as something else.
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
        // Big operators, which Typst puts limits on by itself.
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
        // Functions, which Typst renders upright.
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
        // Mappings and symbols that are their own glyph.
        "infty" => "infinity",
        "partial" => "diff",
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
        // The transpose and perpendicular signs, which are the same two glyphs in both notations
        // and are in every paragraph about a projection.
        "top" => "top",
        "bot" => "bot",
        // The delimiters `\left` and `\right` can name, in the symbols that draw them: Typst's
        // own `{` groups, `|` is a binary operator, and `\|` is a single bar character.
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
        // The commands that mean something different in the two notations, and the ones that mean
        // the same thing and are therefore left alone (D165).
        assert_eq!(latex(r"\frac{a+b}{2}").unwrap(), "frac(a+b, 2)");
        assert_eq!(latex(r"\sqrt{x}").unwrap(), "sqrt(x)");
        assert_eq!(latex(r"\sqrt[3]{x+1}").unwrap(), "root(3, x+1)");
        assert_eq!(latex(r"\alpha \cdot \beta").unwrap(), "alpha dot beta");
        // A rate bound: the epsilon is its own glyph, and the slanted inequalities are the
        // relations Typst spells `gt.tilde` and `lt.tilde` rather than `gtrsim` and `lesssim`.
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
        // Typst has no ≳-or-≈ or ≲-or-≈ glyph, so a guess would draw a different relation.
        assert_eq!(latex(r"a \gtrapprox b"), None);
        assert_eq!(latex(r"x^2 + y_i").unwrap(), "x^2 + y_i");
        assert_eq!(latex(r"\sin^2 x").unwrap(), "sin^2 x");
        // A thin space is a space, and `dx` is two letters written apart, as TeX sets them.
        assert_eq!(
            latex(r"\int_0^1 f(x)\,dx").unwrap(),
            "integral_0^1 f(x) d x"
        );
        assert_eq!(latex(r"\mathbb{R}").unwrap(), "bb(R)");
        assert_eq!(latex(r"\hat{x}").unwrap(), "hat(x)");
        // The transpose and perpendicular signs are the same glyphs in both notations, and both
        // notations spell them the same way.
        assert_eq!(latex(r"r^\top h").unwrap(), "r^top h");
        assert_eq!(latex(r"a \bot b").unwrap(), "a bot b");
        assert_eq!(latex(r"\text{if } x>0").unwrap(), "\"if \" x>0");
        assert_eq!(
            latex(r"a \\ b").unwrap(),
            "a \\ b",
            "a line break stays one"
        );
        // A group is grouping in both, so the argument of a command keeps its shape.
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
        // `aligned` keeps its alignment points and breaks: Typst draws them the same way.
        assert_eq!(
            latex(r"\begin{aligned}a &= b \\ c &= d\end{aligned}").unwrap(),
            "a &= b \\ c &= d"
        );
    }

    #[test]
    fn an_environment_is_refused_when_one_of_its_cells_cannot_be_translated() {
        // A cell left out of a matrix draws a matrix with holes in it, which is the one thing this
        // front end must never do: the source is the honest answer (D165).
        assert_eq!(
            latex(r"\begin{pmatrix}a & b \\ \foo & d\end{pmatrix}"),
            None
        );
        // `\ll` and `\gg` are what physics is written with, and are symbols like any other.
        assert_eq!(latex(r"ka\ll 1"), Some("k a lt.double 1".into()));
        assert_eq!(latex(r"x\gg y"), Some("x gt.double y".into()));
    }

    #[test]
    fn a_command_this_file_does_not_know_is_refused_rather_than_guessed() {
        // The rule that keeps the transcript honest: the source is drawn instead of something
        // wrong, so a formula the renderer cannot handle is visible as its TeX (D165).
        assert_eq!(latex(r"\foo{x}"), None);
        assert_eq!(latex(r"\begin{tikzpicture}\end{tikzpicture}"), None);
        assert_eq!(latex(r"a \over b"), None, "a TeX primitive, not a command");
        // Braces and nesting are handled, including a group inside an argument.
        assert_eq!(
            latex(r"\frac{a}{\frac{b}{c}}").unwrap(),
            "frac(a, frac(b, c))"
        );
    }

    #[test]
    fn a_formula_is_typeset_into_a_picture_at_the_terminal_s_pixel_size() {
        // The end-to-end shape of the feature: an image whose pixels are screen pixels, so the
        // terminal draws it unscaled and the rows reserved for it are exactly its height (D165).
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
        // Two rows tall for a one-line formula, and as many pixels as the box has at twice the
        // resolution: the picture is sharp, and the terminal scales it into the rows.
        assert_eq!(formula.cells.1, 2, "{formula:?}");
        assert!(
            formula.image.height >= 2 * room.cell.1,
            "the raster is finer than the box it fills: {:?}",
            formula.image
        );
        // The same formula at the same room is the same picture: nothing is typeset twice.
        let again = render(r"E = m c^2", &room).expect("a formula renders");
        assert_eq!(formula.image.data, again.image.data);
    }

    #[test]
    fn a_formula_in_a_line_is_one_row_of_cells() {
        // The inline shape: a picture exactly the height of a row, as wide as the formula and the
        // room beside it, drawn into the cells the line reserved for it (D165).
        let room = Room {
            cell: (8, 17),
            cols: 100,
            rows: 1,
            colour: (220, 220, 220),
            scale: 1.0,
        };
        let formula = render_inline(r"a^2 + b^2", &room).expect("a formula renders");
        // A rate bound in a sentence is drawn like any other inline formula (D165): the slanted
        // inequality is a glyph Typst has, not a command it would refuse.
        let bound = render_inline(r"k \epsilon^2 \gtrsim \log n", &room)
            .expect("an inline formula renders");
        assert_eq!(bound.cells.1, 1, "one row: {bound:?}");
        assert!(formula.inline, "it sits inside a line");
        assert_eq!(
            formula.cells.1, 1,
            "one row, whatever it holds: {formula:?}"
        );
        assert!(formula.cells.0 > 2, "as wide as it is: {formula:?}");
        // The raster is finer than the row it fills, so the terminal has more pixels than it shows.
        assert!(
            formula.image.height >= room.cell.1 * RESOLUTION,
            "the raster is finer than the box it fills: {:?}",
            formula.image
        );
        // The picture's proportions are the box's, which is what makes the terminal fit it to the
        // cells rather than to one of them and leave it short of the row. A pixel of rounding on
        // either axis is invisible; a cell's worth is not.
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
        // The reason the page carries a strut at all: an inline formula has to meet the text's
        // baseline, not the middle of the row, and where the baseline is inside the picture is
        // checked by looking at the ink (D165).
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
        // `x` has nothing below the baseline, and room above it for the ascenders beside it.
        assert!((top as f32) < baseline, "above the baseline");
        assert!(top as f32 > height * 0.2, "not at the very top: {top}");
    }

    #[test]
    fn a_formula_in_a_line_too_tall_for_one_row_is_left_to_its_source() {
        // A fraction of a fraction is three lines of maths: shrunk into one row it is unreadable,
        // and a formula nobody can read is worse than the TeX it came from (D165).
        let room = Room {
            cell: (8, 20),
            cols: 100,
            rows: 1,
            colour: (0, 0, 0),
            scale: 1.0,
        };
        assert!(render_inline(r"\frac{1}{1 + \frac{1}{x}}", &room).is_none());
        // One fraction is a line's worth, and is drawn as one.
        assert!(render_inline(r"\frac{a}{b}", &room).is_some());
    }

    /// The rows of a formula's picture that have ink in them.
    fn ink_rows_of(png: &str) -> Option<(u32, u32)> {
        let bytes = general_purpose::STANDARD.decode(png).ok()?;
        let mut reader = png::Decoder::new(bytes.as_slice()).read_info().ok()?;
        let mut buffer = vec![0; reader.output_buffer_size()];
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
        // `[ui] math-scale`: the same formula, the same cell, one number apart. The picture is
        // bigger by that much, and the cache keeps the two apart because the scale is part of what
        // decides it (D165).
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
        // A scale nobody meant is held to a range rather than obeyed: four times is the most, so a
        // typed `math-scale = 500` is not one glyph over the whole transcript.
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
        // A room too small to be legible is not a reason to draw three unreadable pixels.
        assert!(render(r"\frac{a+b+c+d+e+f}{g+h+i+j+k}", &Room::tiny()).is_none());
        // And a formula the front-end does not know never reaches the typesetter.
        assert!(render(r"\foo{x}", &Room::tiny()).is_none());
    }

    #[test]
    fn a_letter_run_in_latex_is_letters_and_the_typst_name_is_a_command() {
        // LaTeX has no multi-letter variable, so `dx` is `d` times `x`; Typst reads `dx` as a name
        // it does not have and refuses the formula. A space in maths is not drawn, so the letters
        // are written apart — and that is also what makes the reading unambiguous (D165).
        assert_eq!(latex("x^2 dx"), Some("x^2 d x".into()));
        assert_eq!(latex("alpha"), Some("a l p h a".into()));
        assert_eq!(latex(r"\alpha"), Some("alpha".into()));
        // A call written where LaTeX is meant is the letters it spells — `frac(a, b)` is `f r a c`
        // of a bracketed `a`, `b`, which is what TeX sets — while the same text in the invocation
        // that means Typst draws the fraction. No body is read both ways (D165).
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
        // The bug this test exists for: `dx` made Typst refuse the whole formula, so an integral —
        // the first thing anyone writes — came out as its source.
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
        // `n` followed by `\to` is `narrow.r` without a space: a name that means something else.
        assert_eq!(latex(r"n\to\infty"), Some("n arrow.r infinity".into()));
        assert_eq!(latex(r"\alpha x"), Some("alpha x".into()));
        assert_eq!(latex(r"\alpha\beta"), Some("alpha beta".into()));
        // Two characters are two variables, written apart so that Typst does not read a name.
        assert_eq!(latex("ab"), Some("a b".into()));
        // A growing delimiter keeps its bracket, and `\left.` keeps none.
        assert_eq!(latex(r"\left( x \right)^n"), Some("( x )^n".into()));
        assert_eq!(latex(r"\left\{ x \right."), Some("brace.l x".into()));
    }
}
