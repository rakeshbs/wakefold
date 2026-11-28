use crate::icons::Icon;
use gpui::{Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};
pub(crate) const INK: u32 = 0x23382f;
pub(crate) const MUTED: u32 = 0x596b62;
pub(crate) const SOFT: u32 = 0x65766e;
pub(crate) const GREEN: u32 = 0x285d48;
pub(crate) const LINE: u32 = 0xdce3dc;
pub(crate) const TINT: u32 = 0xe6eee6;
pub(crate) const PAPER: u32 = 0xfdfdfa;
pub(crate) const RAIL: u32 = 0xf3f5ef;
pub(crate) const WHITE: u32 = 0xffffff;
pub(crate) const AMBER: u32 = 0x876423;

pub(crate) fn column() -> Div {
    div().flex().flex_col()
}
pub(crate) fn row() -> Div {
    div().flex().items_center()
}
pub(crate) fn label(s: impl Into<SharedString>, sz: f32, c: u32) -> Div {
    div().text_size(px(sz)).text_color(rgb(c)).child(s.into())
}
pub(crate) fn heading(s: impl Into<SharedString>, sz: f32) -> Div {
    label(s, sz, INK).font_weight(FontWeight::SEMIBOLD)
}
pub(crate) fn dot(c: u32) -> Div {
    div().size(px(6.)).rounded_full().flex_none().bg(rgb(c))
}
pub(crate) fn hit(id: impl Into<SharedString>) -> Stateful<Div> {
    let id = id.into();
    row()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .cursor_pointer()
        .rounded_md()
}
pub(crate) fn action(id: &'static str, text: &'static str, primary: bool) -> Stateful<Div> {
    hit(id)
        .px_3()
        .h(px(30.))
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .bg(rgb(if primary { GREEN } else { WHITE }))
        .border_1()
        .border_color(rgb(if primary { GREEN } else { LINE }))
        .text_color(rgb(if primary { WHITE } else { INK }))
        .hover(move |s| s.bg(rgb(if primary { 0x36755b } else { TINT })))
        .child(text)
}
pub(crate) fn nav(
    id: &'static str,
    icon: Icon,
    text: &'static str,
    selected: bool,
) -> Stateful<Div> {
    hit(id)
        .gap_3()
        .h(px(35.))
        .px_3()
        .bg(rgb(if selected { TINT } else { RAIL }))
        .hover(|s| s.bg(rgb(TINT)))
        .child(icon.draw(16., GREEN))
        .child(label(text, 13., INK))
}
pub(crate) fn tile(icon: Icon, tint: u32, ink: u32) -> Div {
    row()
        .size(px(31.))
        .justify_center()
        .rounded_md()
        .bg(rgb(tint))
        .flex_none()
        .child(icon.draw(17., ink))
}
