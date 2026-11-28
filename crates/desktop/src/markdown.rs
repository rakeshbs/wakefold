//! Lay out CommonMark blocks with native text wrapping and hanging list markers.
use crate::style::*;
use gpui::{
    Div, FontStyle, FontWeight, HighlightStyle, InteractiveText, StyledText, div, prelude::*, px,
    rgb,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
use std::ops::Range;

#[derive(Default)]
struct Line {
    text: String,
    styles: Vec<(Range<usize>, HighlightStyle)>,
    links: Vec<(Range<usize>, String)>,
    inline: Vec<HighlightStyle>,
    link: Option<(usize, String)>,
}

impl Line {
    fn push(&mut self, value: &str, code: bool) {
        let start = self.text.len();
        self.text.push_str(value);
        let mut style = self.inline.last().copied().unwrap_or_default();
        if code {
            style.background_color = Some(rgb(TINT).into());
        }
        if style != HighlightStyle::default() {
            self.styles.push((start..self.text.len(), style));
        }
    }
    fn flush(&mut self, frame: &mut Block, index: usize) {
        let line = std::mem::take(self);
        if line.text.is_empty() {
            return;
        }
        let text = StyledText::new(line.text).with_highlights(line.styles);
        let element = if line.links.is_empty() {
            text.into_any_element()
        } else {
            let (ranges, urls): (Vec<_>, Vec<_>) = line.links.into_iter().unzip();
            InteractiveText::new(("link", index), text)
                .on_click(ranges, move |i, _, cx| cx.open_url(&urls[i]))
                .into_any_element()
        };
        frame.body.extend([div()
            .flex_none()
            .min_w_0()
            .child(element)
            .into_any_element()]);
    }
}

struct Block {
    body: Div,
    marker: Option<String>,
    // An unordered list has a marker but no number to advance.
    counter: Option<u64>,
}

pub(crate) fn markdown(source: &str) -> Div {
    let root = column()
        .gap_2()
        .min_w_0()
        .text_size(px(15.))
        .line_height(px(24.))
        .text_color(rgb(INK));
    let mut stack = vec![Block {
        body: root,
        marker: None,
        counter: None,
    }];
    let mut line = Line::default();
    for (index, event) in
        Parser::new_ext(source, Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS).enumerate()
    {
        let current = stack.last_mut().expect("document has a root");
        match event {
            Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => line.push(&s, false),
            Event::Code(s) => line.push(&s, true),
            Event::SoftBreak => line.push(" ", false),
            Event::HardBreak => line.push("\n", false),
            Event::TaskListMarker(done) => line.push(if done { "☑ " } else { "☐ " }, false),
            Event::Start(Tag::Strong | Tag::Emphasis | Tag::Link { .. } | Tag::Image { .. }) => {
                let mut style = line.inline.last().copied().unwrap_or_default();
                match event {
                    Event::Start(Tag::Strong) => style.font_weight = Some(FontWeight::BOLD),
                    Event::Start(Tag::Emphasis) => style.font_style = Some(FontStyle::Italic),
                    Event::Start(Tag::Link { dest_url, .. })
                        if dest_url.starts_with("https://") || dest_url.starts_with("http://") =>
                    {
                        style.color = Some(rgb(GREEN).into());
                        line.link = Some((line.text.len(), dest_url.into_string()));
                    }
                    _ => {}
                }
                line.inline.push(style);
            }
            Event::End(
                pulldown_cmark::TagEnd::Link
                | pulldown_cmark::TagEnd::Strong
                | pulldown_cmark::TagEnd::Emphasis
                | pulldown_cmark::TagEnd::Image,
            ) => {
                if matches!(event, Event::End(pulldown_cmark::TagEnd::Link))
                    && let Some((start, url)) = line.link.take()
                {
                    line.links.push((start..line.text.len(), url));
                }
                line.inline.pop();
            }
            Event::Start(tag) => {
                line.flush(current, index);
                let mut marker = None;
                let mut counter = None;
                let mut body = column().flex_none().min_w_0();
                body = match tag {
                    Tag::Heading { level, .. } => {
                        let size = match level {
                            HeadingLevel::H1 => 25.,
                            HeadingLevel::H2 => 21.,
                            _ => 17.,
                        };
                        body.font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(size))
                            .line_height(px(size * 1.35))
                    }
                    Tag::CodeBlock(_) => body
                        .bg(rgb(RAIL))
                        .rounded_lg()
                        .p_3()
                        .text_size(px(13.))
                        .line_height(px(19.))
                        .font_family("monospace"),
                    Tag::BlockQuote(_) => body
                        .border_l_2()
                        .border_color(rgb(LINE))
                        .pl_3()
                        .text_color(rgb(MUTED)),
                    Tag::List(number) => {
                        counter = number;
                        body.gap_1()
                    }
                    Tag::Item => {
                        marker = Some(match &mut current.counter {
                            Some(n) => {
                                let label = format!("{n}.");
                                *n = n.saturating_add(1);
                                label
                            }
                            None => "•".into(),
                        });
                        body.flex_1().gap_1()
                    }
                    Tag::TableHead | Tag::TableRow => {
                        let row = div()
                            .flex()
                            .flex_none()
                            .min_w_0()
                            .border_b_1()
                            .border_color(rgb(LINE));
                        if matches!(tag, Tag::TableHead) {
                            row.font_weight(FontWeight::SEMIBOLD).bg(rgb(RAIL))
                        } else {
                            row
                        }
                    }
                    Tag::TableCell => body.flex_1().p_2(),
                    _ => body,
                };
                stack.push(Block {
                    body,
                    marker,
                    counter,
                });
            }
            Event::End(_) => {
                line.flush(current, index);
                let block = stack.pop().expect("balanced CommonMark blocks");
                let element = if let Some(marker) = block.marker {
                    div()
                        .flex()
                        .flex_none()
                        .items_start()
                        .gap_2()
                        .child(div().flex_none().min_w(px(16.)).child(marker))
                        .child(block.body)
                } else {
                    block.body
                };
                stack
                    .last_mut()
                    .expect("document has a root")
                    .body
                    .extend([element.into_any_element()]);
            }
            Event::Rule => {
                line.flush(current, index);
                current
                    .body
                    .extend([div().h(px(1.)).bg(rgb(LINE)).into_any_element()]);
            }
            _ => {}
        }
    }
    let mut root = stack.pop().expect("document has a root");
    line.flush(&mut root, usize::MAX);
    root.body
}
