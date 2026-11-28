//! One right-hand workspace for file revisions and child conversations.
use super::artifacts::Preview;
use super::*;
use gpui::{CursorStyle, DragMoveEvent, Empty, ScrollHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Panel {
    Artifacts,
    File(i64, u64),
    Child(i64),
}

impl Panel {
    fn id(self) -> String {
        match self {
            Self::Artifacts => "artifacts".into(),
            Self::File(id, rev) => format!("file-{id}-{rev}"),
            Self::Child(id) => format!("agent-{id}"),
        }
    }
}

struct Tab {
    panel: Panel,
    title: String,
    scroll: ScrollHandle,
    preview: Option<Preview>,
    events: VecDeque<Event>,
}

pub(super) struct Dock {
    tabs: Vec<Tab>,
    pub(super) active: Option<Panel>,
    width: f32,
    tab_scroll: ScrollHandle,
}
impl Default for Dock {
    fn default() -> Self {
        Self {
            tabs: vec![],
            active: None,
            width: 440.,
            tab_scroll: ScrollHandle::new(),
        }
    }
}
impl Dock {
    pub(super) fn clear(&mut self) {
        self.tabs.clear();
        self.active = None;
        self.tab_scroll = ScrollHandle::new();
    }
    fn select(&mut self, panel: Panel, title: String) -> bool {
        let existing = self.tabs.iter().position(|t| t.panel == panel);
        let index = existing.unwrap_or_else(|| {
            self.tabs.push(Tab {
                panel,
                title,
                scroll: ScrollHandle::new(),
                preview: None,
                events: VecDeque::new(),
            });
            self.tabs.len() - 1
        });
        self.active = Some(panel);
        self.tab_scroll.scroll_to_item(index);
        existing.is_none()
    }
    pub(super) fn receive_event(&mut self, event: &Event) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.panel == Panel::Child(event.run_id))
            && tab
                .events
                .back()
                .is_none_or(|e| e.sequence < event.sequence)
        {
            if tab.events.len() == 200 {
                tab.events.pop_front();
            }
            tab.events.push_back(event.clone());
        }
    }
    pub(super) fn child_events(&self, id: i64) -> Option<&VecDeque<Event>> {
        self.tabs
            .iter()
            .find(|t| t.panel == Panel::Child(id))
            .map(|t| &t.events)
    }
    #[cfg(test)]
    pub(super) fn preview_text(&self, id: i64, revision: u64) -> Option<&str> {
        let tab = self
            .tabs
            .iter()
            .find(|t| t.panel == Panel::File(id, revision))?;
        match tab.preview.as_ref()? {
            Preview::Text { text, .. } => Some(text),
            _ => None,
        }
    }
    pub(super) fn preview_loaded(&mut self, panel: Panel, preview: Preview) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.panel == panel) {
            tab.preview = Some(preview);
        }
    }
    pub(super) fn follow_child_if_near_bottom(&self) {
        if let Some(tab) = self
            .tabs
            .iter()
            .find(|t| Some(t.panel) == self.active && matches!(t.panel, Panel::Child(_)))
        {
            super::conversation::follow_if_near_bottom(&tab.scroll);
        }
    }
}
#[derive(Clone)]
struct ResizeDock;

impl Servers {
    pub(super) fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        let title = match panel {
            Panel::Artifacts => "Artifacts".into(),
            Panel::Child(id) => {
                let Some(run) = self.runs.iter().find(|r| r.run_id == id) else {
                    return;
                };
                format!("Agent {}", run.agent_id)
            }
            Panel::File(id, rev) => {
                let Some(file) = self
                    .artifacts
                    .revisions
                    .iter()
                    .find(|f| (f.artifact_id, f.revision) == (id, rev))
                else {
                    return;
                };
                format!("{} · r{}", file.title, rev)
            }
        };
        self.conversation.follow_if_near_bottom();
        let new = self.dock.select(panel, title);
        let child = match panel {
            Panel::Child(id) => Some(id),
            _ => None,
        };
        self.remember_chat(child);
        self.follow_run(child.or_else(|| self.latest_root()), cx);
        if new
            && let Panel::File(id, revision) = panel
            && let Some(file) = self
                .artifacts
                .revisions
                .iter()
                .find(|f| (f.artifact_id, f.revision) == (id, revision))
                .cloned()
        {
            self.load_preview(file, cx);
        }
        cx.notify();
    }
    fn close_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        let Some(i) = self.dock.tabs.iter().position(|t| t.panel == panel) else {
            return;
        };
        self.dock.tabs.remove(i);
        if self.dock.active == Some(panel) {
            if let Some(tab) = self.dock.tabs.get(i).or_else(|| self.dock.tabs.last()) {
                let next = tab.panel;
                self.open_panel(next, cx);
            } else {
                self.conversation.follow_if_near_bottom();
                self.dock.active = None;
                self.remember_chat(None);
                self.follow_run(self.latest_root(), cx);
            }
        }
        cx.notify();
    }

    pub(super) fn split_workspace(&self, main: Div, width: f32, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut layout = div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .child(main);
        let Some(tab) = self
            .dock
            .tabs
            .iter()
            .find(|t| Some(t.panel) == self.dock.active)
        else {
            return layout;
        };
        let active = tab.panel;
        let mut strip = row()
            .id("workspace-tabs")
            .flex_none()
            .h(px(40.))
            .min_w_0()
            .track_scroll(&self.dock.tab_scroll)
            .overflow_x_scroll()
            .border_b_1()
            .border_color(rgb(colors.border));
        for t in &self.dock.tabs {
            let panel = t.panel;
            let selected = active == panel;
            strip = strip.child(
                row()
                    .flex_none()
                    .h_full()
                    .border_b_2()
                    .border_color(rgb(if selected {
                        colors.accent
                    } else {
                        colors.surface
                    }))
                    .child(
                        hit(format!("tab-{}", panel.id()))
                            .h_full()
                            .px_2()
                            .max_w(px(162.))
                            .child(
                                label(
                                    t.title.clone(),
                                    12.,
                                    if selected { colors.text } else { colors.muted },
                                )
                                .truncate(),
                            )
                            .on_click(cx.listener(move |s, _, _, cx| s.open_panel(panel, cx))),
                    )
                    .child(
                        hit(format!("close-tab-{}", panel.id()))
                            .p_1()
                            .child(Icon::Close.draw(12., colors.muted))
                            .on_click(cx.listener(move |s, _, _, cx| s.close_panel(panel, cx))),
                    ),
            );
        }
        let body = match active {
            Panel::Artifacts => self.artifact_library(cx),
            Panel::File(id, revision) => self.file_preview(id, revision, tab.preview.as_ref(), cx),
            Panel::Child(id) => self
                .runs
                .iter()
                .find(|r| r.run_id == id)
                .map(|r| self.child_panel(r, cx))
                .unwrap_or_else(|| label("Loading agent…", 14., colors.muted)),
        };
        let close_id = if matches!(active, Panel::Child(_)) {
            "close-child"
        } else {
            "close-panel"
        };
        let overlay = width < 720.;
        let pane_width = if overlay {
            self.dock.width.min(width - 20.)
        } else {
            self.dock.width.min((width - 360.).max(320.))
        };
        let mut pane = column()
            .id("workspace-panel")
            .debug_selector(|| "workspace-panel".into())
            .relative()
            .flex_none()
            .w(px(pane_width))
            .h_full()
            .min_h_0()
            .bg(rgb(colors.surface))
            .border_l_1()
            .border_color(rgb(colors.border))
            .child(strip)
            .child(
                row()
                    .h(px(48.))
                    .px_4()
                    .gap_2()
                    .flex_none()
                    .border_b_1()
                    .border_color(rgb(colors.border))
                    .child(
                        heading(colors, tab.title.clone(), 15.)
                            .truncate()
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        hit(close_id)
                            .p_2()
                            .child(Icon::Close.draw(16., colors.muted))
                            .on_click(cx.listener(move |s, _, _, cx| s.close_panel(active, cx))),
                    ),
            )
            .child(crate::scrollbar::scroll_region(
                "panel-scrollbar",
                column()
                    .id(gpui::SharedString::from(format!("scroll-{}", active.id())))
                    .debug_selector(|| "panel-scroll".into())
                    .track_scroll(&tab.scroll)
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(body.flex_none().w_full()),
                &tab.scroll,
            ));
        if overlay {
            pane = pane
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .shadow_lg()
                .occlude();
        } else {
            pane = pane.child(
                div()
                    .id("resize-workspace")
                    .debug_selector(|| "resize-workspace".into())
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(6.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .hover(|d| d.bg(rgb(colors.tint)))
                    .on_drag(ResizeDock, |_, _, _, cx| cx.new(|_| Empty))
                    .on_drag_move(cx.listener(|s, event: &DragMoveEvent<ResizeDock>, w, cx| {
                        s.conversation.follow_if_near_bottom();
                        s.dock.width = f32::from(w.viewport_size().width - event.event.position.x)
                            .clamp(320., 760.);
                        cx.notify();
                    })),
            );
        }
        layout = layout.child(pane);
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        MouseButton, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, point, size,
    };
    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let point = cx.debug_bounds(selector).unwrap().center();
        cx.simulate_click(point, Default::default());
        cx.run_until_parked();
    }
    fn wheel(cx: &mut VisualTestContext, selector: &'static str, dy: f32) {
        let position = cx.debug_bounds(selector).unwrap().center();
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            ..Default::default()
        });
        cx.run_until_parked();
    }
    #[gpui::test]
    fn scrolling_and_resizing_keep_the_chat_and_workspace_independent(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let (view,cx)=cx.add_window_view(|_,cx| {
            let mut s=Servers::with_settings(Runtime::new().unwrap(),Some(directory.path().join("settings.json")),cx);
            s.load=None;
            s.chat=Some(Uuid::new_v4());
            let run=Run{ run_id:1,agent_id:1,parent_agent_id:None,spawned_by_run_id:None,run_key:"first".into(),latest_event:"run_finished".into(),outcome:Some("## Findings\n\n1. First point\n   - Nested **detail**\n\n```\n    source indentation\n```\n".repeat(12)),task:"Compare the two options.".into(),status:"completed".into()};
            let mut child=run.clone();
            child.run_id=2;child.agent_id=2;child.parent_agent_id=Some(1);child.spawned_by_run_id=Some(1);
            s.apply(Update::Snapshot{ chats:vec![],runs:vec![run,child],approvals:vec![],artifacts:vec![] },cx);
            s
        });
        cx.simulate_resize(size(px(1360.), px(745.)));
        cx.run_until_parked();
        let composer = cx.debug_bounds("composer").unwrap();
        let feed = cx.debug_bounds("conversation").unwrap();
        assert!(feed.bottom() <= composer.top());
        let scroll = cx.update(|_, app| view.read(app).conversation.scroll.clone());
        assert!(scroll.max_offset().height > px(500.));
        assert!(
            (scroll.offset().y + scroll.max_offset().height).abs() < px(1.),
            "initial offset {:?}, maximum {:?}",
            scroll.offset(),
            scroll.max_offset()
        );
        wheel(cx, "conversation", 380.);
        let reading = scroll.offset().y;
        assert!(reading + scroll.max_offset().height > px(300.));
        cx.update(|_, app| {
            view.update(app, |s, cx| {
                let mut runs = s.runs.clone();
                runs[0]
                    .outcome
                    .as_mut()
                    .unwrap()
                    .push_str("\n\nAnother paragraph");
                s.apply(
                    Update::Snapshot {
                        chats: vec![],
                        runs,
                        artifacts: vec![],
                        approvals: vec![],
                    },
                    cx,
                );
                cx.notify();
            })
        });
        cx.run_until_parked();
        assert_eq!(scroll.offset().y, reading);
        assert_eq!(cx.debug_bounds("composer").unwrap(), composer);
        let rail = cx.debug_bounds("chat-scrollbar").unwrap();
        cx.simulate_click(
            point(rail.center().x, rail.top() + px(2.)),
            Default::default(),
        );
        cx.run_until_parked();
        assert_eq!(scroll.offset().y, px(0.));
        let grab = point(rail.center().x, rail.top() + px(4.));
        cx.simulate_mouse_down(grab, MouseButton::Left, Default::default());
        for dy in [8., 80.] {
            cx.simulate_mouse_move(
                grab + point(px(0.), px(dy)),
                MouseButton::Left,
                Default::default(),
            );
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(
            grab + point(px(0.), px(80.)),
            MouseButton::Left,
            Default::default(),
        );
        cx.run_until_parked();
        assert!(scroll.offset().y < px(-10.));
        let reading = scroll.offset().y;
        click(cx, "chat-artifacts");
        cx.update(|_, app| view.update(app, |s, cx| s.open_panel(Panel::Child(2), cx)));
        cx.run_until_parked();
        let dock = cx.debug_bounds("workspace-panel").unwrap();
        assert!(cx.debug_bounds("composer").unwrap().right() <= dock.left());
        assert!(cx.debug_bounds("tab-artifacts").is_some());
        let panel_scroll = cx.update(|_, app| view.read(app).dock.tabs[1].scroll.clone());
        assert!(panel_scroll.max_offset().height > px(400.));
        wheel(cx, "panel-scroll", -180.);
        assert!(panel_scroll.offset().y < px(-100.));
        assert_eq!(scroll.offset().y, reading);
        click(cx, "tab-artifacts");
        click(cx, "tab-agent-2");
        assert!(panel_scroll.offset().y < px(-100.));
        let handle = cx.debug_bounds("resize-workspace").unwrap().center();
        cx.simulate_mouse_down(handle, MouseButton::Left, Default::default());
        for x in [12., 90.] {
            cx.simulate_mouse_move(
                handle - point(px(x), px(0.)),
                MouseButton::Left,
                Default::default(),
            );
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(
            handle - point(px(90.), px(0.)),
            MouseButton::Left,
            Default::default(),
        );
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-panel").unwrap().size.width > dock.size.width + px(50.));
        click(cx, "close-child");
        assert!(cx.debug_bounds("workspace-panel").is_some());
        click(cx, "close-panel");
        assert_eq!(cx.update(|_, app| view.read(app).dock.active), None);
        assert_eq!(cx.debug_bounds("composer").unwrap(), composer);
    }
}
