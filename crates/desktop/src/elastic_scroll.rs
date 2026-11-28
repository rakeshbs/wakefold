//! GPUI keeps the real scroll position bounded. Only the painted children move past an edge.
use gpui::{
    Along, AnyElement, App, Axis, Bounds, Div, ElementId, GlobalElementId, IntoElement, LayoutId,
    Pixels, Point, ScrollHandle, Stateful, StyleRefinement, TouchPhase, Window, prelude::*, px,
};
use gpui_component::scroll::Scrollbar;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Spring {
    touching: bool,
    pull: f32,
    release: Option<Instant>,
}

impl Spring {
    fn current_pull(&self, now: Instant) -> f32 {
        let Some(start) = self.release else {
            return self.pull;
        };
        let t = now.saturating_duration_since(start).as_secs_f32() * 22.;
        self.pull * (1. + t) * (-t).exp()
    }

    fn absorb(
        &mut self,
        attempted: Pixels,
        max: Pixels,
        viewport: Pixels,
        phase: TouchPhase,
        precise: bool,
        now: Instant,
    ) -> Pixels {
        let distance = attempted + px(self.current_pull(now));
        let bounded = distance.clamp(-max, px(0.));
        self.pull = f32::from(distance - bounded).clamp(-f32::from(viewport), f32::from(viewport));
        match phase {
            TouchPhase::Started => self.touching = precise,
            TouchPhase::Ended => self.touching = false,
            TouchPhase::Moved if !precise => self.touching = false,
            TouchPhase::Moved => {}
        }
        // An explicit finger gesture stays stretched until lift. Wheel input and momentum
        // streams without phases settle shortly after the last delta.
        self.release = if matches!(phase, TouchPhase::Ended) {
            Some(now)
        } else if self.touching {
            None
        } else {
            Some(now + Duration::from_millis(if precise { 100 } else { 65 }))
        };
        bounded
    }

    fn displacement(
        &mut self,
        position: Pixels,
        max: Pixels,
        viewport: Pixels,
        now: Instant,
    ) -> Pixels {
        let pull = self.current_pull(now);
        if pull.abs() < 0.25
            || max <= px(0.)
            || (pull > 0. && position < px(0.))
            || (pull < 0. && position > -max)
        {
            self.pull = 0.;
            return px(0.);
        }
        let limit = (f32::from(viewport) * 0.30).min(88.);
        let resisted = pull * 0.55;
        px(resisted * limit / (limit + resisted.abs()))
    }
}

#[derive(Default)]
struct ScrollState {
    handle: ScrollHandle,
    spring: Spring,
}

pub(crate) trait ScrollExt {
    fn elastic_y(self, handle: Option<&ScrollHandle>) -> ElasticScroll;
    fn elastic_x(self, handle: Option<&ScrollHandle>) -> ElasticScroll;
}
impl ScrollExt for Stateful<Div> {
    fn elastic_y(self, handle: Option<&ScrollHandle>) -> ElasticScroll {
        ElasticScroll::new(self.overflow_y_scroll(), Axis::Vertical, handle)
    }
    fn elastic_x(self, handle: Option<&ScrollHandle>) -> ElasticScroll {
        ElasticScroll::new(self.overflow_x_scroll(), Axis::Horizontal, handle)
    }
}

#[derive(IntoElement)]
pub(crate) struct ElasticScroll {
    viewport: Stateful<Div>,
    axis: Axis,
    handle: Option<ScrollHandle>,
    children: Vec<AnyElement>,
    thumb: Option<&'static str>,
}
impl ElasticScroll {
    fn new(viewport: Stateful<Div>, axis: Axis, handle: Option<&ScrollHandle>) -> Self {
        Self {
            viewport,
            axis,
            handle: handle.cloned(),
            children: vec![],
            thumb: None,
        }
    }
    pub(crate) fn with_scrollbar(mut self, id: &'static str) -> Self {
        self.thumb = Some(id);
        self
    }
}
impl Styled for ElasticScroll {
    fn style(&mut self) -> &mut StyleRefinement {
        self.viewport.style()
    }
}
impl ParentElement for ElasticScroll {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for ElasticScroll {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let axis = self.axis;
        let id = Element::id(&self.viewport).expect("stateful scroll viewport has an ID");
        let state = window.with_id("elastic-scroll", |w| {
            w.use_keyed_state(id, cx, |_, _| ScrollState::default())
        });
        let handle = self.handle.unwrap_or_else(|| state.read(cx).handle.clone());
        let now = cx.background_executor().now();
        let shift = state.update(cx, |s, _| {
            s.spring.displacement(
                handle.offset().along(axis),
                handle.max_offset().along(axis),
                handle.bounds().size.along(axis),
                now,
            )
        });
        let mut viewport = self.viewport.track_scroll(&handle);
        if shift == px(0.) {
            viewport.extend(self.children);
        } else {
            let translation = Point::default().apply_along(axis, |_| shift);
            viewport.extend(
                self.children
                    .into_iter()
                    .map(|child| Shifted { child, translation }.into_any_element()),
            );
            if state.read(cx).spring.release.is_some() {
                window.request_animation_frame();
            }
        }
        let wheel_handle = handle.clone();
        let wheel_state = state.clone();
        let viewport = viewport.on_scroll_wheel(move |event, window, cx| {
            let max = wheel_handle.max_offset().along(axis);
            if max <= px(0.) {
                return;
            }
            // GPUI's wheel listener has already added this event's platform-scaled delta.
            // Capture the excess here before GPUI clamps the offset at the next layout.
            let attempted = wheel_handle.offset();
            let now = cx.background_executor().now();
            let bounded = wheel_state.update(cx, |s, _| {
                s.spring.absorb(
                    attempted.along(axis),
                    max,
                    wheel_handle.bounds().size.along(axis),
                    event.touch_phase,
                    event.delta.precise(),
                    now,
                )
            });
            wheel_handle.set_offset(attempted.apply_along(axis, |_| bounded));
            cx.stop_propagation();
            window.refresh();
        });
        let hover_state = state.clone();
        let viewport = viewport.on_hover(move |inside, window, cx| {
            if !inside && hover_state.read(cx).spring.touching {
                let now = cx.background_executor().now();
                hover_state.update(cx, |s, _| {
                    s.spring.touching = false;
                    s.spring.release = Some(now);
                });
                window.refresh();
            }
        });
        if let Some(id) = self.thumb {
            crate::theme::ensure_scrollbar_theme(cx);
            let scrollbar = Scrollbar::vertical(&handle);
            gpui::div()
                .id(id)
                .debug_selector(move || id.into())
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .capture_any_mouse_down(move |_, _, cx| {
                    if state.read(cx).spring.pull != 0. {
                        state.update(cx, |state, cx| {
                            state.spring = Spring::default();
                            cx.notify();
                        });
                    }
                })
                .child(viewport)
                .child(scrollbar)
                .into_any_element()
        } else {
            viewport.into_any_element()
        }
    }
}

// Delegate layout directly so ScrollHandle's item indices, flex sizing, and text wrapping survive
// the translation. Prepaint moves hit testing, selection, and painted content together.
struct Shifted {
    child: AnyElement,
    translation: Point<Pixels>,
}
impl IntoElement for Shifted {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for Shifted {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_element_offset(self.translation, |w| self.child.prepaint(w, cx));
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, div, point, size,
    };

    #[derive(Default)]
    struct ScrollExample {
        vertical: ScrollHandle,
        tabs: ScrollHandle,
    }

    impl Render for ScrollExample {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let strip = div()
                .flex()
                .id("tabs")
                .w(px(240.))
                .h(px(40.))
                .flex_none()
                .elastic_x(Some(&self.tabs))
                .child(
                    div()
                        .id("tab")
                        .debug_selector(|| "tab".into())
                        .w(px(600.))
                        .h_full()
                        .flex_none(),
                );
            div().size_full().flex().flex_col().child(
                div()
                    .id("outer")
                    .flex()
                    .flex_col()
                    .elastic_y(Some(&self.vertical))
                    .child(strip)
                    .child(
                        div()
                            .id("body")
                            .debug_selector(|| "body".into())
                            .h(px(1000.))
                            .flex_none(),
                    )
                    .with_scrollbar("track"),
            )
        }
    }

    fn wheel(
        cx: &mut VisualTestContext,
        position: Point<Pixels>,
        delta: ScrollDelta,
        phase: TouchPhase,
    ) {
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta,
            touch_phase: phase,
            ..Default::default()
        });
    }
    fn tick(cx: &mut VisualTestContext, millis: u64) {
        cx.background_executor
            .advance_clock(Duration::from_millis(millis));
        cx.update(|w, _| w.refresh());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn trackpad_stretches_and_releases_the_inner_tab_strip(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| ScrollExample::default());
        cx.simulate_resize(size(px(320.), px(180.)));
        cx.run_until_parked();
        let rest = cx.debug_bounds("tab").unwrap().left();
        let at = point(px(80.), px(20.));
        wheel(
            cx,
            at,
            ScrollDelta::Pixels(point(px(75.), px(12.))),
            TouchPhase::Started,
        );
        let pulled = cx.debug_bounds("tab").unwrap().left() - rest;
        assert!(
            pulled > px(5.) && pulled < px(75.),
            "resisted distance {pulled:?}"
        );
        cx.update(|_, app| {
            let view = view.read(app);
            assert_eq!(view.tabs.offset(), Point::default());
            assert_eq!(view.vertical.offset(), Point::default());
        });
        tick(cx, 450);
        assert_eq!(cx.debug_bounds("tab").unwrap().left() - rest, pulled);
        wheel(
            cx,
            at,
            ScrollDelta::Pixels(Point::default()),
            TouchPhase::Ended,
        );
        tick(cx, 90);
        let returning = cx.debug_bounds("tab").unwrap().left() - rest;
        assert!(returning > px(0.) && returning < pulled);
        tick(cx, 600);
        assert_eq!(cx.debug_bounds("tab").unwrap().left(), rest);
        wheel(
            cx,
            at,
            ScrollDelta::Pixels(point(px(-50.), px(0.))),
            TouchPhase::Moved,
        );
        cx.update(|_, app| assert_eq!(view.read(app).tabs.offset().x, px(-50.)));
    }

    #[gpui::test]
    fn wheels_over_the_scrollbar_bounce_at_the_bottom_and_settle_without_an_end_event(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| ScrollExample::default());
        cx.simulate_resize(size(px(320.), px(180.)));
        cx.run_until_parked();
        let handle = cx.update(|w, app| {
            let handle = view.read(app).vertical.clone();
            handle.scroll_to_bottom();
            w.refresh();
            handle
        });
        cx.run_until_parked();
        let bottom = cx.debug_bounds("body").unwrap().bottom();
        let bounds = cx.debug_bounds("track").unwrap();
        let track = point(bounds.right() - px(6.), bounds.center().y);
        wheel(
            cx,
            track,
            ScrollDelta::Lines(point(0., -3.)),
            TouchPhase::Moved,
        );
        assert!(cx.debug_bounds("body").unwrap().bottom() < bottom - px(3.));
        assert_eq!(handle.offset().y, -handle.max_offset().height);
        tick(cx, 650);
        assert_eq!(cx.debug_bounds("body").unwrap().bottom(), bottom);
        wheel(
            cx,
            track,
            ScrollDelta::Lines(point(0., 1.)),
            TouchPhase::Moved,
        );
        assert!(handle.offset().y > -handle.max_offset().height);
    }
}
