//! The composer uses GPUI's wrapped text layout for painting, mouse selection and IME coordinates.
use super::*;
use crate::elastic_scroll::ScrollExt;
use gpui::{ScrollHandle, StyledText, TextLayout, canvas};

#[derive(Default)]
pub(super) struct Multiline {
    pub(super) layout: Option<TextLayout>,
    scroll: ScrollHandle,
    pub(super) reveal_caret: bool,
}

impl TextInput {
    pub(super) fn wrapped(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> crate::elastic_scroll::ElasticScroll {
        let colors = crate::theme::palette(cx);
        let editor = self.multiline.as_ref().expect("chat composer");
        let display: SharedString = if self.content.is_empty() {
            self.placeholder.clone()
        } else if self.content.ends_with('\n') {
            // Give the final empty line a measurable caret position.
            format!("{}\u{200b}", self.content).into()
        } else {
            self.content.clone()
        };
        let style = window.text_style();
        let base = TextRun {
            len: 0,
            font: style.font(),
            color: if self.content.is_empty() {
                rgb(colors.secondary).into()
            } else {
                rgb(colors.text).into()
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let mut boundaries = vec![
            0,
            display.len(),
            self.selected_range.start,
            self.selected_range.end,
        ];
        if let Some(mark) = &self.marked_range {
            boundaries.extend([mark.start, mark.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let runs = boundaries
            .windows(2)
            .map(|b| {
                let mark = self
                    .marked_range
                    .as_ref()
                    .is_some_and(|r| r.contains(&b[0]));
                TextRun {
                    len: b[1] - b[0],
                    background_color: self
                        .selected_range
                        .contains(&b[0])
                        .then(|| rgba(colors.selection).into()),
                    underline: mark.then_some(UnderlineStyle {
                        color: Some(base.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..base.clone()
                }
            })
            .collect();
        let text = StyledText::new(display).with_runs(runs);
        let layout = text.layout().clone();
        let input = cx.entity();
        let scroll = editor.scroll.clone();
        let paint_scroll = scroll.clone();
        div()
            .id("wrapped-composer")
            .w_full()
            .min_h(px(32.))
            .max_h(px(120.))
            .elastic_y(Some(&scroll))
            .px_1()
            .py_1()
            .child(
                div().relative().w_full().child(text).child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, w, cx| {
                            let viewport = paint_scroll.bounds();
                            let field = input.read(cx);
                            let focus = field.focus_handle.clone();
                            w.handle_input(
                                &focus,
                                ElementInputHandler::new(viewport, input.clone()),
                                cx,
                            );
                            let caret = layout.position_for_index(field.cursor_offset());
                            let reveal = field.multiline.as_ref().is_some_and(|m| m.reveal_caret);
                            if let Some(pos) = caret
                                && focus.is_focused(w)
                            {
                                if field.selected_range.is_empty() {
                                    w.paint_quad(fill(
                                        Bounds::new(pos, size(px(2.), layout.line_height())),
                                        rgb(colors.accent),
                                    ));
                                }
                                if reveal {
                                    let shift = if pos.y < viewport.top() {
                                        viewport.top() - pos.y
                                    } else {
                                        (viewport.bottom() - pos.y - layout.line_height())
                                            .min(px(0.))
                                    };
                                    if shift != px(0.) {
                                        let offset = paint_scroll.offset();
                                        paint_scroll.set_offset(point(
                                            offset.x,
                                            (offset.y + shift)
                                                .clamp(-paint_scroll.max_offset().height, px(0.)),
                                        ));
                                        w.refresh();
                                    }
                                }
                            }
                            input.update(cx, |field, _| {
                                if let Some(m) = &mut field.multiline {
                                    m.layout = Some(layout);
                                    m.reveal_caret = false;
                                }
                            });
                        },
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
    }
    pub(super) fn vertical_move(&mut self, direction: f32, cx: &mut Context<Self>) {
        let Some(layout) = self.multiline.as_ref().and_then(|m| m.layout.as_ref()) else {
            return;
        };
        let Some(pos) = layout.position_for_index(self.cursor_offset()) else {
            return;
        };
        let p = pos + point(px(0.), layout.line_height() * (direction + 0.5));
        let offset = layout
            .index_for_position(p)
            .unwrap_or_else(|nearest| nearest)
            .min(self.content.len());
        self.move_to(offset, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn composer_preserves_pasted_indentation_and_scrolls_long_drafts(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new(cx));
        cx.simulate_resize(size(px(400.), px(300.)));
        cx.update(|w, app| w.focus(&input.read(app).focus_handle(app)));
        cx.simulate_input("Notes");
        cx.simulate_keystrokes("shift-enter");
        cx.update(|_, app| {
            app.write_to_clipboard(ClipboardItem::new_string("  - one\n    - two 🦋".into()))
        });
        cx.dispatch_action(Paste);
        cx.run_until_parked();
        cx.update(|_, app| assert_eq!(input.read(app).text(), "Notes\n  - one\n    - two 🦋"));
        cx.simulate_input(&"\nMore notes.".repeat(20));
        cx.run_until_parked();
        cx.update(|_, app| {
            let field = input.read(app);
            let editor = field.multiline.as_ref().unwrap();
            assert!(editor.scroll.bounds().size.height <= px(120.));
            assert!(editor.scroll.offset().y < px(0.));
            assert!(
                editor
                    .layout
                    .as_ref()
                    .unwrap()
                    .position_for_index(field.text().len())
                    .unwrap()
                    .y
                    <= editor.scroll.bounds().bottom()
            );
        });
    }
}
