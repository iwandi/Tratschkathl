//! Terminal model for the Agent Handler: alacritty_terminal VT state, key encoding using the
//! current terminal modes, and conversion of the visible grid into a [`Frame`] for the UI.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

use tk_bus::frame::{Frame, KeyInput, Run, BOLD, ITALIC, UNDERLINE};

pub const DEFAULT_FG: u32 = 0xd4d4d4;
pub const DEFAULT_BG: u32 = 0x1e1e1e;

#[derive(Clone)]
struct Listener(Arc<Mutex<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct TermModel {
    term: Term<Listener>,
    parser: Processor,
    events: Arc<Mutex<Vec<Event>>>,
    pub title: Option<String>,
}

impl TermModel {
    pub fn new(cols: u16, rows: u16) -> Self {
        let events = Arc::new(Mutex::new(vec![]));
        let size = Size { cols: cols.max(2) as usize, rows: rows.max(2) as usize };
        let term = Term::new(Config::default(), &size, Listener(events.clone()));
        TermModel { term, parser: Processor::new(), events, title: None }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    /// Feed PTY output. Returns bytes the terminal wants to write back (query answers).
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        let events: Vec<Event> = std::mem::take(&mut *self.events.lock().unwrap());
        let mut reply = Vec::new();
        for e in events {
            match e {
                Event::PtyWrite(s) => reply.extend_from_slice(s.as_bytes()),
                Event::Title(t) => self.title = Some(t),
                Event::ResetTitle => self.title = None,
                Event::ColorRequest(idx, fmt) => {
                    let c = palette(idx);
                    reply.extend_from_slice(fmt(Rgb { r: (c >> 16) as u8, g: (c >> 8) as u8, b: c as u8 }).as_bytes());
                }
                _ => {}
            }
        }
        reply
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size { cols: cols.max(2) as usize, rows: rows.max(2) as usize });
    }

    pub fn scroll(&mut self, delta: i32) {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
        } else {
            text.into_bytes()
        }
    }

    /// Encode a key press using the current terminal modes.
    pub fn key_bytes(&self, k: &KeyInput) -> Option<Vec<u8>> {
        let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
        let m = 1 + (k.shift as u8) + 2 * (k.alt as u8) + 4 * (k.ctrl as u8);
        let csi = |letter: char| -> Vec<u8> {
            if m > 1 {
                format!("\x1b[1;{m}{letter}").into_bytes()
            } else if app_cursor {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        };
        let tilde = |n: u8| -> Vec<u8> {
            if m > 1 { format!("\x1b[{n};{m}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
        };
        let bytes = match k.key.as_str() {
            "enter" => if k.alt { b"\x1b\r".to_vec() } else { b"\r".to_vec() },
            "backspace" => {
                if k.ctrl { b"\x08".to_vec() } else if k.alt { b"\x1b\x7f".to_vec() } else { b"\x7f".to_vec() }
            }
            "tab" => if k.shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() },
            "escape" => b"\x1b".to_vec(),
            "space" => if k.ctrl { vec![0] } else if k.alt { b"\x1b ".to_vec() } else { b" ".to_vec() },
            "up" => csi('A'),
            "down" => csi('B'),
            "right" => csi('C'),
            "left" => csi('D'),
            "home" => csi('H'),
            "end" => csi('F'),
            "insert" => tilde(2),
            "delete" => tilde(3),
            "pageup" => tilde(5),
            "pagedown" => tilde(6),
            "f1" => b"\x1bOP".to_vec(),
            "f2" => b"\x1bOQ".to_vec(),
            "f3" => b"\x1bOR".to_vec(),
            "f4" => b"\x1bOS".to_vec(),
            "f5" => tilde(15),
            "f6" => tilde(17),
            "f7" => tilde(18),
            "f8" => tilde(19),
            "f9" => tilde(20),
            "f10" => tilde(21),
            "f11" => tilde(23),
            "f12" => tilde(24),
            key => {
                let mut chars = key.chars();
                let single = match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                };
                if k.ctrl && !k.alt {
                    let c = single?.to_ascii_lowercase();
                    match c {
                        'a'..='z' => vec![c as u8 - b'a' + 1],
                        '@' | '2' => vec![0],
                        '[' | '3' => vec![0x1b],
                        '\\' | '4' => vec![0x1c],
                        ']' | '5' => vec![0x1d],
                        '^' | '6' => vec![0x1e],
                        '_' | '-' | '7' => vec![0x1f],
                        _ => return None,
                    }
                } else if let Some(text) = &k.key_char {
                    if k.alt && !k.ctrl {
                        let mut v = vec![0x1b];
                        v.extend_from_slice(text.as_bytes());
                        v
                    } else {
                        text.as_bytes().to_vec()
                    }
                } else {
                    return None;
                }
            }
        };
        Some(bytes)
    }

    /// The visible grid as styled runs, with a block cursor baked in.
    pub fn frame(&self) -> Frame {
        let content = self.term.renderable_content();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let offset = content.display_offset as i32;
        let colors = content.colors;
        let cursor = content.cursor;
        let show_cursor = cursor.shape != CursorShape::Hidden && offset == 0;
        let mut lines: Vec<Vec<Run>> = vec![Vec::new(); rows];

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = indexed.cell;
            let row = point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(cell.fg, colors, true);
            let mut bg = resolve(cell.bg, colors, false);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if show_cursor && point == cursor.point {
                std::mem::swap(&mut fg, &mut bg);
                if fg == bg {
                    bg = DEFAULT_FG;
                    fg = DEFAULT_BG;
                }
            }
            let mut flags = 0;
            if cell.flags.contains(Flags::BOLD) {
                flags |= BOLD;
            }
            if cell.flags.contains(Flags::ITALIC) {
                flags |= ITALIC;
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                flags |= UNDERLINE;
            }
            let mut text = String::new();
            text.push(if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c });
            if let Some(zw) = cell.zerowidth() {
                text.extend(zw.iter());
            }
            let cells = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let line = &mut lines[row as usize];
            match line.last_mut() {
                Some(last) if last.fg == fg && last.bg == bg && last.flags == flags => {
                    last.text.push_str(&text);
                    last.cells += cells;
                }
                _ => line.push(Run { text, cells, fg, bg, flags }),
            }
        }
        // Trim trailing default-background whitespace to keep frames small.
        for line in lines.iter_mut() {
            if let Some(last) = line.last_mut() {
                if last.bg == DEFAULT_BG && last.flags & UNDERLINE == 0 {
                    let trimmed = last.text.trim_end_matches(' ').len();
                    let removed = (last.text.len() - trimmed) as u16;
                    last.text.truncate(trimmed);
                    last.cells = last.cells.saturating_sub(removed);
                    if last.text.is_empty() {
                        line.pop();
                    }
                }
            }
        }
        Frame { cols: cols as u16, rows: rows as u16, display_offset: offset as u32, lines }
    }
}

fn resolve(c: Color, colors: &alacritty_terminal::term::color::Colors, fg: bool) -> u32 {
    let from_rgb = |r: Rgb| ((r.r as u32) << 16) | ((r.g as u32) << 8) | r.b as u32;
    match c {
        Color::Spec(rgb) => from_rgb(rgb),
        Color::Indexed(i) => colors[i as usize].map(from_rgb).unwrap_or_else(|| palette(i as usize)),
        Color::Named(n) => {
            let idx = n as usize;
            if let Some(rgb) = colors[idx] {
                return from_rgb(rgb);
            }
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
                NamedColor::Background => DEFAULT_BG,
                NamedColor::DimForeground => dim(DEFAULT_FG),
                NamedColor::Cursor => DEFAULT_FG,
                _ if idx < 16 => palette(idx),
                _ if idx >= NamedColor::DimBlack as usize && idx <= NamedColor::DimWhite as usize => {
                    dim(palette(idx - NamedColor::DimBlack as usize))
                }
                _ => if fg { DEFAULT_FG } else { DEFAULT_BG },
            }
        }
    }
}

fn dim(c: u32) -> u32 {
    let f = |v: u32| (v * 2 / 3) & 0xff;
    (f(c >> 16) << 16) | (f((c >> 8) & 0xff) << 8) | f(c & 0xff)
}

/// xterm-256 palette (VS Code-like first 16).
pub fn palette(i: usize) -> u32 {
    const BASE: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5,
        0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    match i {
        0..=15 => BASE[i],
        16..=231 => {
            let i = i - 16;
            let v = |x: usize| if x == 0 { 0 } else { (x * 40 + 55) as u32 };
            (v(i / 36) << 16) | (v((i / 6) % 6) << 8) | v(i % 6)
        }
        232..=255 => {
            let g = (8 + (i - 232) * 10) as u32;
            (g << 16) | (g << 8) | g
        }
        256 => DEFAULT_FG,
        257 => DEFAULT_BG,
        _ => DEFAULT_FG,
    }
}
