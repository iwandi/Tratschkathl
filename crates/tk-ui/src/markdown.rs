//! Markdown document renderer: CommonMark blocks (headings, paragraphs, lists, code, quotes,
//! tables, rules) with inline bold / italic / code / links via `StyledText` highlights.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{div, px, rgb, AnyElement, FontStyle, FontWeight, HighlightStyle, SharedString, StyledText};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const TEXT: u32 = 0xcccccc;
const DIM: u32 = 0x888888;
const CODE_BG: u32 = 0x2a2a2a;
const CODE_FG: u32 = 0xce9178;
const LINK: u32 = 0x3b8eea;

enum Block {
    Heading(u8, String, Vec<(Range<usize>, HighlightStyle)>),
    Para(String, Vec<(Range<usize>, HighlightStyle)>),
    Item(usize, String, Vec<(Range<usize>, HighlightStyle)>),
    Quote(String, Vec<(Range<usize>, HighlightStyle)>),
    Code(String),
    Rule,
}

#[derive(Default)]
struct Inline {
    text: String,
    hl: Vec<(Range<usize>, HighlightStyle)>,
    open: Vec<(usize, HighlightStyle)>,
}

impl Inline {
    fn start(&mut self, style: HighlightStyle) {
        self.open.push((self.text.len(), style));
    }
    fn end(&mut self) {
        if let Some((start, style)) = self.open.pop() {
            if start < self.text.len() {
                self.hl.push((start..self.text.len(), style));
            }
        }
    }
}

fn bold() -> HighlightStyle {
    HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() }
}
fn italic() -> HighlightStyle {
    HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() }
}
fn code_style() -> HighlightStyle {
    HighlightStyle { color: Some(rgb(CODE_FG).into()), background_color: Some(rgb(CODE_BG).into()), ..Default::default() }
}
fn link() -> HighlightStyle {
    HighlightStyle { color: Some(rgb(LINK).into()), ..Default::default() }
}

fn parse(text: &str) -> Vec<Block> {
    let mut blocks = vec![];
    let mut inl = Inline::default();
    let mut list_depth = 0usize;
    let mut in_item = 0usize;
    let mut in_quote = 0usize;
    let mut in_code = false;
    let mut code = String::new();
    let mut table: Vec<String> = vec![];
    let mut row: Vec<String> = vec![];
    let mut in_table = false;

    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Trim only the end of the text, so highlight ranges stay valid.
    let flush = |inl: &mut Inline| -> (String, Vec<(Range<usize>, HighlightStyle)>) {
        inl.open.clear();
        let mut t = std::mem::take(&mut inl.text);
        let end = t.trim_end().len();
        t.truncate(end);
        let hl = std::mem::take(&mut inl.hl).into_iter().filter(|(r, _)| r.start < end).map(|(r, s)| (r.start..r.end.min(end), s)).collect();
        (t, hl)
    };

    for ev in Parser::new_ext(text, opts) {
        match ev {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                code.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                blocks.push(Block::Code(code.trim_end().to_string()));
            }
            Event::Text(t) if in_code => code.push_str(&t),
            Event::Start(Tag::Table(_)) => {
                in_table = true;
                table.clear();
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                blocks.push(Block::Code(table.join("\n")));
            }
            Event::Start(Tag::TableRow) | Event::Start(Tag::TableHead) => row.clear(),
            Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                table.push(format!("| {} |", row.join(" | ")));
            }
            Event::End(TagEnd::TableCell) => {
                let (t, _) = flush(&mut inl);
                row.push(t);
            }
            Event::Start(Tag::Heading { .. }) => {
                inl = Inline::default();
            }
            Event::End(TagEnd::Heading(level)) => {
                let (t, hl) = flush(&mut inl);
                blocks.push(Block::Heading(level as u8, t, hl));
            }
            Event::Start(Tag::List(_)) => {
                // An item's own text comes before its nested list: emit it at its depth.
                if in_item > 0 && !inl.text.trim().is_empty() {
                    let (t, hl) = flush(&mut inl);
                    blocks.push(Block::Item(list_depth.saturating_sub(1), t, hl));
                }
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            Event::Start(Tag::Item) => {
                if in_item > 0 && !inl.text.trim().is_empty() {
                    let (t, hl) = flush(&mut inl);
                    blocks.push(Block::Item(list_depth.saturating_sub(1), t, hl));
                }
                in_item += 1;
            }
            Event::End(TagEnd::Item) => {
                let (t, hl) = flush(&mut inl);
                if !t.is_empty() {
                    blocks.push(Block::Item(list_depth.saturating_sub(1), t, hl));
                }
                in_item = in_item.saturating_sub(1);
            }
            Event::Start(Tag::BlockQuote(_)) => in_quote += 1,
            Event::End(TagEnd::BlockQuote(_)) => in_quote = in_quote.saturating_sub(1),
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if in_item > 0 || in_table {
                    inl.text.push(' ');
                } else {
                    let (t, hl) = flush(&mut inl);
                    if in_quote > 0 {
                        blocks.push(Block::Quote(t, hl));
                    } else {
                        blocks.push(Block::Para(t, hl));
                    }
                }
            }
            Event::Start(Tag::Strong) => inl.start(bold()),
            Event::Start(Tag::Emphasis) => inl.start(italic()),
            Event::Start(Tag::Link { .. }) => inl.start(link()),
            Event::End(TagEnd::Strong) | Event::End(TagEnd::Emphasis) | Event::End(TagEnd::Link) => inl.end(),
            Event::Text(t) => inl.text.push_str(&t),
            Event::Code(t) => {
                let start = inl.text.len();
                inl.text.push_str(&t);
                inl.hl.push((start..inl.text.len(), code_style()));
            }
            Event::TaskListMarker(done) => inl.text.push_str(if done { "☑ " } else { "☐ " }),
            Event::SoftBreak => inl.text.push(' '),
            Event::HardBreak => inl.text.push('\n'),
            Event::Rule => blocks.push(Block::Rule),
            _ => {}
        }
    }
    blocks
}

fn styled(text: String, hl: Vec<(Range<usize>, HighlightStyle)>) -> StyledText {
    StyledText::new(SharedString::from(text)).with_highlights(hl)
}

pub fn render(text: &str, mono: SharedString) -> AnyElement {
    let blocks = parse(text);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(rgb(TEXT))
        .children(blocks.into_iter().map(|b| match b {
            Block::Heading(level, t, hl) => {
                let size = match level {
                    1 => 24.0,
                    2 => 20.0,
                    3 => 17.0,
                    _ => 15.0,
                };
                div()
                    .pt(px(if level <= 2 { 10.0 } else { 4.0 }))
                    .text_size(px(size))
                    .font_weight(FontWeight::BOLD)
                    .when(level <= 2, |d| d.pb_1().border_b_1().border_color(rgb(0x333333)))
                    .child(styled(t, hl))
                    .into_any_element()
            }
            Block::Para(t, hl) => div().child(styled(t, hl)).into_any_element(),
            Block::Item(depth, t, hl) => div()
                .pl(px(8.0 + 18.0 * depth as f32))
                .flex()
                .flex_row()
                .gap_2()
                .child(div().flex_none().text_color(rgb(DIM)).child("•"))
                .child(div().flex_1().min_w_0().child(styled(t, hl)))
                .into_any_element(),
            Block::Quote(t, hl) => div()
                .pl_3()
                .border_l_2()
                .border_color(rgb(0x555555))
                .text_color(rgb(DIM))
                .child(styled(t, hl))
                .into_any_element(),
            Block::Code(t) => div()
                .p_2()
                .rounded_md()
                .bg(rgb(CODE_BG))
                .font_family(mono.clone())
                .text_size(px(13.0))
                .flex()
                .flex_col()
                .children(t.lines().map(|l| div().whitespace_nowrap().child(l.replace('\t', "    "))).collect::<Vec<_>>())
                .into_any_element(),
            Block::Rule => div().h(px(1.0)).bg(rgb(0x333333)).into_any_element(),
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_blocks() {
        let md = "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n  - nested\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quote\n\n---\n";
        let blocks = parse(md);
        assert!(matches!(blocks[0], Block::Heading(1, ref t, _) if t == "Title"));
        match &blocks[1] {
            Block::Para(t, hl) => {
                assert_eq!(t, "Some bold and code.");
                assert_eq!(hl.len(), 2);
            }
            _ => panic!("expected paragraph"),
        }
        let items: Vec<_> = blocks.iter().filter_map(|b| if let Block::Item(d, t, _) = b { Some((*d, t.clone())) } else { None }).collect();
        assert_eq!(items, vec![(0, "one".to_string()), (0, "two".to_string()), (1, "nested".to_string())]);
        assert!(blocks.iter().any(|b| matches!(b, Block::Code(t) if t == "fn main() {}")));
        assert!(blocks.iter().any(|b| matches!(b, Block::Code(t) if t.contains("| a | b |"))));
        assert!(blocks.iter().any(|b| matches!(b, Block::Quote(t, _) if t == "quote")));
        assert!(blocks.iter().any(|b| matches!(b, Block::Rule)));
    }
}
