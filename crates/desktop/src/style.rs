use crate::icons::Icon;
use gpui::{Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};

pub(crate) fn column() -> Div {
    div().flex().flex_col()
}
pub(crate) fn row() -> Div {
    div().flex().items_center()
}
/// Keeps the library scrollbar fixed beside its independently scrolling content.
pub(crate) fn scroll_pane(
    id: &'static str,
    content: impl IntoElement,
    handle: &(impl gpui::component::scroll::ScrollbarHandle + Clone),
) -> Stateful<Div> {
    use gpui::component::scroll::ScrollableElement;
    column()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .child(gpui::base::ScrollBounce::new("bounce", handle, content))
        .vertical_scrollbar(handle)
}

pub(crate) fn label(s: impl Into<SharedString>, sz: f32, c: u32) -> Div {
    div().text_size(px(sz)).text_color(rgb(c)).child(s.into())
}
pub(crate) fn heading(colors: crate::theme::Palette, s: impl Into<SharedString>, sz: f32) -> Div {
    label(s, sz, colors.text).font_weight(FontWeight::SEMIBOLD)
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
pub(crate) fn action(
    colors: crate::theme::Palette,
    id: &'static str,
    text: &'static str,
    primary: bool,
) -> Stateful<Div> {
    hit(id)
        .px_3()
        .h(px(30.))
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .bg(rgb(if primary {
            colors.accent
        } else {
            colors.surface
        }))
        .border_1()
        .border_color(rgb(if primary {
            colors.accent
        } else {
            colors.border
        }))
        .text_color(rgb(if primary {
            colors.on_accent
        } else {
            colors.text
        }))
        .hover(move |s| {
            s.bg(rgb(if primary {
                colors.accent_hover
            } else {
                colors.tint
            }))
        })
        .child(text)
}
pub(crate) fn nav(
    colors: crate::theme::Palette,
    id: &'static str,
    icon: Icon,
    text: &'static str,
    selected: bool,
) -> Stateful<Div> {
    hit(id)
        .gap_3()
        .h(px(35.))
        .px_3()
        .bg(rgb(if selected {
            colors.tint
        } else {
            colors.sidebar
        }))
        .hover(move |s| s.bg(rgb(colors.tint)))
        .child(icon.draw(16., colors.accent))
        .child(label(text, 13., colors.text))
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
