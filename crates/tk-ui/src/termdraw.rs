//! Terminal line rendering. Text in the terminal font is laid out in runs, but two kinds of
//! characters are positioned per cell, like real terminal emulators do:
//!
//! - Block elements (U+2580–U+259F) and box drawing (U+2500–U+257F) are drawn as geometry that
//!   fills the whole cell, so they connect across cells and rows (logos, borders) no matter
//!   which font the system would fall back to.
//! - Any other non-ASCII glyph gets its own box of exactly its terminal width, so a wider
//!   fallback glyph (emoji, symbols) cannot shift the rest of the line.

use gpui::prelude::*;
use gpui::{div, px, rgb, rgba, AnyElement, Div, FontWeight};
use unicode_width::UnicodeWidthChar;

use tk_bus::frame::{Run, BOLD, ITALIC, UNDERLINE};

pub fn render_line(line: &[Run], cell_w: f32, line_h: f32, term_bg: u32) -> AnyElement {
    let mut row = div().h(px(line_h)).flex().flex_row().whitespace_nowrap();
    for r in line {
        row = render_run(row, r, cell_w, line_h, term_bg);
    }
    row.into_any_element()
}

fn styled(mut d: Div, r: &Run, term_bg: u32) -> Div {
    d = d.text_color(rgb(r.fg));
    if r.bg != term_bg {
        d = d.bg(rgb(r.bg));
    }
    if r.flags & BOLD != 0 {
        d = d.font_weight(FontWeight::BOLD);
    }
    if r.flags & ITALIC != 0 {
        d = d.italic();
    }
    if r.flags & UNDERLINE != 0 {
        d = d.underline();
    }
    d
}

fn text_box(text: String, cells: f32, r: &Run, cell_w: f32, line_h: f32, term_bg: u32) -> Div {
    styled(div().flex_none().w(px(cells * cell_w)).h(px(line_h)).overflow_hidden(), r, term_bg).child(text)
}

fn render_run(mut row: Div, r: &Run, cell_w: f32, line_h: f32, term_bg: u32) -> Div {
    let width = |c: char| c.width().unwrap_or(0);
    // Fast path: plain ASCII. Also used when our widths disagree with the terminal's count.
    let simple = r.text.is_ascii() || r.text.chars().map(width).sum::<usize>() != r.cells as usize;
    if simple {
        let cells = if r.cells > 0 { r.cells as f32 } else { r.text.chars().count() as f32 };
        return row.child(text_box(r.text.clone(), cells, r, cell_w, line_h, term_bg));
    }
    let mut ascii = String::new();
    let flush = |row: Div, ascii: &mut String| -> Div {
        if ascii.is_empty() {
            return row;
        }
        let n = ascii.chars().count() as f32;
        row.child(text_box(std::mem::take(ascii), n, r, cell_w, line_h, term_bg))
    };
    for c in r.text.chars() {
        let w = width(c);
        if c.is_ascii() || w == 0 {
            ascii.push(c);
            continue;
        }
        row = flush(row, &mut ascii);
        let cell = styled(div().flex_none().relative().w(px(w as f32 * cell_w)).h(px(line_h)).overflow_hidden(), r, term_bg);
        row = row.child(match geometry(c, r.fg, cell_w, line_h) {
            Some(parts) => cell.children(parts),
            None => cell.child(c.to_string()),
        });
    }
    flush(row, &mut ascii)
}

/// A filled rectangle inside the cell (fractions of the cell size).
fn rect(x: f32, y: f32, w: f32, h: f32, color: u32, alpha: u32) -> AnyElement {
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .bg(rgba((color << 8) | alpha))
        .into_any_element()
}

/// Geometry for block elements and box drawing, or `None` for ordinary glyphs.
fn geometry(c: char, fg: u32, cw: f32, ch: f32) -> Option<Vec<AnyElement>> {
    let full = |x0: f32, y0: f32, x1: f32, y1: f32| rect(x0 * cw, y0 * ch, (x1 - x0) * cw, (y1 - y0) * ch, fg, 0xff);
    let q = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut v = vec![];
        if ul {
            v.push(full(0.0, 0.0, 0.5, 0.5));
        }
        if ur {
            v.push(full(0.5, 0.0, 1.0, 0.5));
        }
        if ll {
            v.push(full(0.0, 0.5, 0.5, 1.0));
        }
        if lr {
            v.push(full(0.5, 0.5, 1.0, 1.0));
        }
        v
    };
    let code = c as u32;
    let parts = match code {
        0x2580 => vec![full(0.0, 0.0, 1.0, 0.5)],
        0x2581..=0x2587 => {
            let eighths = (code - 0x2580) as f32;
            vec![full(0.0, 1.0 - eighths / 8.0, 1.0, 1.0)]
        }
        0x2588 => vec![full(0.0, 0.0, 1.0, 1.0)],
        0x2589..=0x258f => {
            let eighths = (0x2590 - code) as f32;
            vec![full(0.0, 0.0, eighths / 8.0, 1.0)]
        }
        0x2590 => vec![full(0.5, 0.0, 1.0, 1.0)],
        0x2591..=0x2593 => {
            let alpha = [0x40, 0x80, 0xc0][(code - 0x2591) as usize];
            vec![rect(0.0, 0.0, cw, ch, fg, alpha)]
        }
        0x2594 => vec![full(0.0, 0.0, 1.0, 0.125)],
        0x2595 => vec![full(0.875, 0.0, 1.0, 1.0)],
        0x2596 => q(false, false, true, false),
        0x2597 => q(false, false, false, true),
        0x2598 => q(true, false, false, false),
        0x2599 => q(true, false, true, true),
        0x259a => q(true, false, false, true),
        0x259b => q(true, true, true, false),
        0x259c => q(true, true, false, true),
        0x259d => q(false, true, false, false),
        0x259e => q(false, true, true, false),
        0x259f => q(false, true, true, true),
        0x2500..=0x257f => box_lines(c, fg, cw, ch)?,
        _ => return None,
    };
    Some(parts)
}

/// Line weights (left, right, up, down): 0 none, 1 light, 2 heavy, 3 double.
fn box_arms(c: char) -> Option<[u8; 4]> {
    Some(match c {
        '─' | '┄' | '┈' | '╌' => [1, 1, 0, 0],
        '━' | '┅' | '┉' | '╍' => [2, 2, 0, 0],
        '│' | '┆' | '┊' | '╎' => [0, 0, 1, 1],
        '┃' | '┇' | '┋' | '╏' => [0, 0, 2, 2],
        '┌' | '╭' => [0, 1, 0, 1],
        '┐' | '╮' => [1, 0, 0, 1],
        '└' | '╰' => [0, 1, 1, 0],
        '┘' | '╯' => [1, 0, 1, 0],
        '┏' => [0, 2, 0, 2],
        '┓' => [2, 0, 0, 2],
        '┗' => [0, 2, 2, 0],
        '┛' => [2, 0, 2, 0],
        '├' => [0, 1, 1, 1],
        '┤' => [1, 0, 1, 1],
        '┬' => [1, 1, 0, 1],
        '┴' => [1, 1, 1, 0],
        '┼' => [1, 1, 1, 1],
        '┣' => [0, 2, 2, 2],
        '┫' => [2, 0, 2, 2],
        '┳' => [2, 2, 0, 2],
        '┻' => [2, 2, 2, 0],
        '╋' => [2, 2, 2, 2],
        '═' => [3, 3, 0, 0],
        '║' => [0, 0, 3, 3],
        '╔' => [0, 3, 0, 3],
        '╗' => [3, 0, 0, 3],
        '╚' => [0, 3, 3, 0],
        '╝' => [3, 0, 3, 0],
        '╠' => [0, 3, 3, 3],
        '╣' => [3, 0, 3, 3],
        '╦' => [3, 3, 0, 3],
        '╩' => [3, 3, 3, 0],
        '╬' => [3, 3, 3, 3],
        '╴' => [1, 0, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 0, 1],
        _ => return None,
    })
}

fn box_lines(c: char, fg: u32, cw: f32, ch: f32) -> Option<Vec<AnyElement>> {
    let [l, r, u, d] = box_arms(c)?;
    let light = (cw / 8.0).round().max(1.0);
    let cx = (cw / 2.0).floor();
    let cy = (ch / 2.0).floor();
    let mut v = vec![];
    // Each arm runs from the cell edge through the centre, so arms meet without gaps.
    let mut arm = |weight: u8, horizontal: bool, toward_start: bool| {
        if weight == 0 {
            return;
        }
        let t = if weight == 2 { light * 2.0 } else { light };
        let offsets: Vec<f32> = if weight == 3 { vec![-light, light] } else { vec![0.0] };
        for o in offsets {
            if horizontal {
                let y = cy - t / 2.0 + o;
                let (x0, x1) = if toward_start { (0.0, cx + t / 2.0) } else { (cx - t / 2.0, cw) };
                v.push(rect(x0, y, x1 - x0, t, fg, 0xff));
            } else {
                let x = cx - t / 2.0 + o;
                let (y0, y1) = if toward_start { (0.0, cy + t / 2.0) } else { (cy - t / 2.0, ch) };
                v.push(rect(x, y0, t, y1 - y0, fg, 0xff));
            }
        }
    };
    arm(l, true, true);
    arm(r, true, false);
    arm(u, false, true);
    arm(d, false, false);
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_logo_and_borders_are_geometry() {
        for c in "▐▛███▜▌▝▜█████▛▘▘▘ ▝▝".chars().filter(|c| *c != ' ') {
            assert!(geometry(c, 0xffffff, 8.0, 18.0).is_some(), "{c} should be drawn as geometry");
        }
        for c in "─│╭╮╯╰├┤┼━┃═║".chars() {
            assert!(geometry(c, 0xffffff, 8.0, 18.0).is_some(), "{c} should be drawn as geometry");
        }
        assert!(geometry('a', 0xffffff, 8.0, 18.0).is_none());
        assert!(geometry('⏵', 0xffffff, 8.0, 18.0).is_none());
    }

    #[test]
    fn quadrants_cover_the_right_corners() {
        // ▛ = upper left, upper right, lower left.
        assert_eq!(geometry('▛', 0xffffff, 8.0, 18.0).unwrap().len(), 3);
        assert_eq!(geometry('▘', 0xffffff, 8.0, 18.0).unwrap().len(), 1);
        assert_eq!(box_arms('┼'), Some([1, 1, 1, 1]));
    }
}
