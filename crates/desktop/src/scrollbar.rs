//! A vertical thumb for a GPUI ScrollHandle. Wheel and drag share one offset.
use gpui::{
    Bounds, Context, Div, DragMoveEvent, Empty, MouseButton, Pixels, ScrollHandle, canvas, div,
    fill, point, prelude::*, px, rgb, size,
};
use std::{cell::Cell, rc::Rc};

#[derive(Clone)]
pub(crate) struct ScrollThumb {
    handle: ScrollHandle,
    grab: Rc<Cell<Pixels>>,
}

impl ScrollThumb {
    fn geometry(&self) -> (Pixels, Pixels, Pixels) {
        let view = self.handle.bounds().size.height;
        let maximum = self.handle.max_offset().height;
        if maximum <= px(0.) || view <= px(0.) {
            return (px(0.), px(0.), px(0.));
        }
        let height = (view * (view / (view + maximum))).max(px(24.)).min(view);
        let travel = view - height;
        (
            height,
            travel * (-self.handle.offset().y / maximum).clamp(0., 1.),
            travel,
        )
    }
    fn move_to(&self, window_y: Pixels) {
        let (_, _, travel) = self.geometry();
        if travel > px(0.) {
            let fraction =
                ((window_y - self.handle.bounds().top() - self.grab.get()) / travel).clamp(0., 1.);
            self.handle.set_offset(point(
                self.handle.offset().x,
                -self.handle.max_offset().height * fraction,
            ));
        }
    }
}
pub(crate) fn drag(
    event: &DragMoveEvent<ScrollThumb>,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) {
    event.drag(cx).move_to(event.event.position.y);
    window.refresh();
}
pub(crate) fn scroll_region(
    id: &'static str,
    content: impl IntoElement,
    handle: &ScrollHandle,
) -> Div {
    let thumb = ScrollThumb {
        handle: handle.clone(),
        grab: Rc::default(),
    };
    let paint = thumb.clone();
    let wheel = handle.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .child(content)
        .child(
            div()
                .id(id)
                .debug_selector(move || id.into())
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w(px(9.))
                .on_drag(thumb.clone(), |thumb, position, _, cx| {
                    let (height, top, _) = thumb.geometry();
                    thumb.grab.set((position.y - top).clamp(px(0.), height));
                    cx.new(|_: &mut Context<Empty>| Empty)
                })
                .on_mouse_down(MouseButton::Left, move |event, window, _| {
                    let (height, top, _) = thumb.geometry();
                    let y = event.position.y - thumb.handle.bounds().top();
                    thumb.grab.set(if y >= top && y <= top + height {
                        y - top
                    } else {
                        height / 2.
                    });
                    thumb.move_to(event.position.y);
                    window.refresh();
                })
                .on_scroll_wheel(move |event, window, _| {
                    let y = (wheel.offset().y + event.delta.pixel_delta(px(20.)).y)
                        .clamp(-wheel.max_offset().height, px(0.));
                    wheel.set_offset(point(wheel.offset().x, y));
                    window.refresh();
                })
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let colors = crate::theme::palette(cx);
                            let (height, top, _) = paint.geometry();
                            if height > px(0.) {
                                let rect = Bounds::new(
                                    bounds.origin + point(px(2.), top),
                                    size(px(4.), height),
                                );
                                window.paint_quad(
                                    fill(rect, rgb(colors.scroll_thumb)).corner_radii(px(2.)),
                                );
                            }
                        },
                    )
                    .size_full(),
                ),
        )
}
