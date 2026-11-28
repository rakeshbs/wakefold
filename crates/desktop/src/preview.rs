mod model;
use crate::{
    SendMessage,
    avatar::{Artwork, Character},
    icons::Icon,
    input::TextInput,
    style::*,
};
use gpui::{
    ClipboardItem, Context, Div, Entity, Focusable, FontWeight, Render, Stateful, Window, div,
    prelude::*, px, rgb,
};
#[cfg(test)]
use gpui::{KeyBinding, size};
use model::{Artifact, Chat, Publication, Tab, Turn};
pub(crate) struct Desktop {
    art: std::rc::Rc<Artwork>,
    welcome: Entity<Character>,
    portrait: Entity<Character>,
    chats: Vec<Chat>,
    current: usize,
    library: bool,
    searching: bool,
    search: Entity<TextInput>,
    show_tools: bool,
}
impl Desktop {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let art = Artwork::load();
        let welcome = cx.new(|_| Character::new(art.clone(), (224., 174.)));
        let portrait = cx.new(|_| Character::new(art.clone(), (58., 50.)));
        let search = cx.new(|cx| TextInput::with_placeholder("Search chats…", cx));
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        let chats = model::examples(cx);
        Self {
            art,
            welcome,
            portrait,
            chats,
            current: 1,
            library: false,
            searching: false,
            search,
            show_tools: false,
        }
    }
    fn chat(&self) -> &Chat {
        &self.chats[self.current]
    }
    fn select(&mut self, i: usize, cx: &mut Context<Self>) {
        self.current = i;
        self.library = false;
        if self.chat().turns.is_empty() {
            self.welcome.update(cx, |a, cx| a.play(0., 2.05, cx));
        } else {
            self.portrait.update(cx, |a, cx| a.play(0., 2.05, cx));
        }
        cx.notify()
    }
    fn new_chat(&mut self, win: &mut Window, cx: &mut Context<Self>) {
        self.chats.push(Chat::new("New chat", cx));
        self.searching = false;
        self.select(self.chats.len() - 1, cx);
        win.focus(&self.chat().draft.focus_handle(cx));
    }
    fn send(&mut self, cx: &mut Context<Self>) {
        if self.library {
            return;
        }
        let chat = &mut self.chats[self.current];
        let msg = chat.draft.read(cx).text().trim().to_owned();
        if msg.is_empty() {
            return;
        }
        if chat.turns.is_empty() {
            chat.title = msg.chars().take(45).collect();
        }
        chat.turns.push(Turn::message(msg));
        chat.scroll.scroll_to_bottom();
        chat.draft.update(cx, |d, cx| {
            d.reset();
            cx.notify();
        });
        cx.notify()
    }
    fn open(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.library = false;
        self.chats[self.current].open(tab);
        cx.notify()
    }
    fn resolve(&mut self, t: usize, yes: bool, cx: &mut Context<Self>) {
        if let Some((_, s)) = &mut self.chats[self.current].turns[t].artifact
            && *s == Publication::Pending
        {
            *s = if yes {
                Publication::Published
            } else {
                Publication::Declined
            };
            self.portrait.update(cx, |a, cx| a.play(3.15, 6.05, cx));
            cx.notify()
        }
    }
    fn sidebar(&self, narrow: bool, cx: &mut Context<Self>) -> Div {
        let brand = row()
            .gap_2()
            .px_2()
            .h(px(48.))
            .child(
                row()
                    .size(px(32.))
                    .rounded_lg()
                    .justify_center()
                    .bg(rgb(GREEN))
                    .child(Icon::Fold.draw(23., 0xf0f5df)),
            )
            .child(heading("Wakefold", 22.));
        let mut rail = column()
            .w(px(if narrow { 170. } else { 212. }))
            .h_full()
            .flex_none()
            .px_3()
            .pt_3()
            .bg(rgb(RAIL))
            .border_r_1()
            .border_color(rgb(LINE))
            .child(brand)
            .child(div().h(px(20.)).flex_none())
            .child(
                nav("new", Icon::New, "New chat", false)
                    .on_click(cx.listener(|s, _, w, cx| s.new_chat(w, cx))),
            )
            .child(
                nav("search", Icon::Search, "Search", self.searching).on_click(cx.listener(
                    |s, _, w, cx| {
                        s.searching = !s.searching;
                        if s.searching {
                            w.focus(&s.search.focus_handle(cx));
                        }
                        cx.notify()
                    },
                )),
            )
            .child(
                nav("history", Icon::History, "History", false).on_click(cx.listener(
                    |s, _, _, cx| {
                        s.library = false;
                        s.searching = false;
                        cx.notify()
                    },
                )),
            )
            .child(
                nav("library", Icon::Library, "Artifacts", self.library).on_click(cx.listener(
                    |s, _, _, cx| {
                        s.library = true;
                        cx.notify()
                    },
                )),
            );
        if self.searching {
            rail = rail.child(
                div()
                    .mt_2()
                    .p_1()
                    .rounded_md()
                    .bg(rgb(WHITE))
                    .border_1()
                    .border_color(rgb(LINE))
                    .child(self.search.clone()),
            );
        }
        let q = if self.searching {
            self.search.read(cx).text().to_lowercase()
        } else {
            String::new()
        };
        let mut list = column()
            .id("recent-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1();
        let mut any = false;
        for (i, c) in self
            .chats
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, c)| c.title.to_lowercase().contains(&q))
        {
            any = true;
            let current = !self.library && self.current == i;
            list = list.child(
                hit(format!("chat-{i}"))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(if current { TINT } else { RAIL }))
                    .hover(|s| s.bg(rgb(TINT)))
                    .child(dot(if c.waiting() { 0xa37c3d } else { 0x92a599 }))
                    .child(
                        label(c.title.clone(), 13., if current { GREEN } else { MUTED }).truncate(),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| s.select(i, cx))),
            );
        }
        if !any {
            list = list.child(label("No matching chats", 12., MUTED).px_3());
        }
        rail.child(
            label("Recent chats", 11., MUTED)
                .font_weight(FontWeight::SEMIBOLD)
                .px_3()
                .mt_6()
                .mb_2(),
        )
        .child(list)
        .child(
            row()
                .gap_2()
                .py_4()
                .px_2()
                .border_t_1()
                .border_color(rgb(LINE))
                .child(tile(Icon::Folder, 0xe3e9de, GREEN))
                .child(column().child(heading("Local workspace", 12.)).child(label(
                    "Design preview",
                    11.,
                    SOFT,
                ))),
        )
    }
    fn activity(&self, t: usize, cx: &mut Context<Self>) -> Div {
        let turn = &self.chat().turns[t];
        let mut box_ = column().gap_2();
        box_ = box_.child(
            hit(format!("activity-{t}"))
                .gap_2()
                .px_1()
                .py_1()
                .hover(|s| s.bg(rgb(TINT)))
                .child(
                    if turn.expanded {
                        Icon::Down
                    } else {
                        Icon::Right
                    }
                    .draw(13., MUTED),
                )
                .child(heading("Activity", 12.))
                .child(label(format!("{} agents", turn.workers.len()), 12., MUTED))
                .on_click(cx.listener(move |s, _, _, cx| {
                    let expanded = &mut s.chats[s.current].turns[t].expanded;
                    *expanded = !*expanded;
                    cx.notify()
                })),
        );
        if turn.expanded {
            let mut cards = row()
                .flex_wrap()
                .gap_2()
                .pl_3()
                .border_l_1()
                .border_color(rgb(LINE));
            for (i, worker) in turn.workers.iter().enumerate() {
                cards = cards.child(
                    hit(format!("worker-{t}-{i}"))
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .bg(rgb(WHITE))
                        .border_1()
                        .border_color(rgb(LINE))
                        .hover(|s| s.bg(rgb(TINT)))
                        .child(tile(
                            Icon::Branch,
                            if i % 2 == 0 { 0xe6eee3 } else { 0xe5edf3 },
                            if i % 2 == 0 { GREEN } else { 0x536e7b },
                        ))
                        .child(
                            column()
                                .child(heading(worker.name, 12.))
                                .child(label("Finished", 10., MUTED)),
                        )
                        .child(Icon::Right.draw(12., SOFT))
                        .on_click(cx.listener(move |s, _, _, cx| s.open(Tab::Worker(t, i), cx))),
                );
            }
            box_ = box_.child(cards);
        }
        box_
    }
    fn file_link(
        &self,
        chat: usize,
        t: usize,
        file: &Artifact,
        id: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        hit(id)
            .gap_3()
            .px_3()
            .py_2()
            .bg(rgb(WHITE))
            .border_1()
            .border_color(rgb(LINE))
            .hover(|s| s.bg(rgb(TINT)))
            .child(tile(Icon::File, TINT, GREEN))
            .child(
                column()
                    .flex_1()
                    .min_w_0()
                    .child(heading(file.filename, 13.).truncate())
                    .child(label("Markdown  ·  Revision 1", 11., MUTED)),
            )
            .child(Icon::Right.draw(14., SOFT))
            .on_click(cx.listener(move |s, _, _, cx| {
                if s.current != chat {
                    s.select(chat, cx);
                }
                s.open(Tab::Artifact(t), cx)
            }))
    }
    fn publication(
        &self,
        t: usize,
        file: &Artifact,
        state: Publication,
        cx: &mut Context<Self>,
    ) -> Div {
        match state {
            Publication::Pending => column()
                .p_3()
                .gap_2()
                .rounded_lg()
                .bg(rgb(0xfaf6ec))
                .border_1()
                .border_color(rgb(0xe6d8b9))
                .child(
                    row()
                        .gap_2()
                        .child(Icon::Gate.draw(16., AMBER))
                        .child(heading("Publish the brief?", 14.)),
                )
                .child(label(
                    format!("Save {} to this chat’s artifacts.", file.filename),
                    12.,
                    0x685b42,
                ))
                .child(
                    row()
                        .gap_2()
                        .child(
                            action("approve", "Approve once", true)
                                .on_click(cx.listener(move |s, _, _, cx| s.resolve(t, true, cx))),
                        )
                        .child(
                            action("decline", "Decline", false)
                                .on_click(cx.listener(move |s, _, _, cx| s.resolve(t, false, cx))),
                        ),
                ),
            Publication::Published => {
                column().child(self.file_link(self.current, t, file, format!("attachment-{t}"), cx))
            }
            Publication::Declined => row()
                .gap_2()
                .child(Icon::Gate.draw(14., MUTED))
                .child(label("Publication declined", 12., MUTED)),
        }
    }
    fn turn(
        &self,
        t: usize,
        latest_reply: Option<usize>,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let turn = &self.chat().turns[t];
        let mut group = column().gap_5().flex_none().child(
            row().justify_end().child(
                label(turn.prompt.clone(), 15., INK)
                    .max_w(px(if narrow { 385. } else { 550. }))
                    .rounded_xl()
                    .px_4()
                    .py_3()
                    .bg(rgb(0xeaf0e8))
                    .line_height(px(22.)),
            ),
        );
        if let Some(reply) = turn.reply {
            let mut body = column()
                .flex_1()
                .min_w_0()
                .gap_3()
                .child(heading("Wakefold", 14.));
            if !turn.workers.is_empty() {
                body = body.child(self.activity(t, cx));
            }
            body = body.child(label(reply, 15., INK).line_height(px(23.)));
            if let Some((file, state)) = turn.artifact {
                body = body.child(self.publication(t, file, state, cx));
            }
            let figure = if latest_reply == Some(t) {
                self.portrait.clone().into_any_element()
            } else {
                self.art.picture(2.05, 58., 50.).into_any_element()
            };
            group = group.child(div().flex().items_start().gap_2().child(figure).child(body));
        } else {
            group = group.child(row().justify_end().child(label(
                "Saved in this local preview",
                11.,
                SOFT,
            )));
        }
        group
    }
    fn conversation(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        let chat = self.chat();
        let pad = if compact { 18. } else { 38. };
        let header = row()
            .justify_between()
            .gap_2()
            .flex_none()
            .px(px(pad))
            .h(px(66.))
            .border_b_1()
            .border_color(rgb(LINE))
            .child(
                column()
                    .min_w_0()
                    .child(heading(chat.title.clone(), 16.).truncate())
                    .child(label("Root agent", 11., MUTED)),
            )
            .child(
                hit("workspace")
                    .gap_2()
                    .px_3()
                    .h(px(32.))
                    .flex_none()
                    .border_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(WHITE))
                    .hover(|s| s.bg(rgb(TINT)))
                    .child(Icon::Folder.draw(15., GREEN))
                    .child(label("Workspace", 12., INK))
                    .on_click(cx.listener(|s, _, _, cx| s.open(Tab::Workspace, cx))),
            );
        let mut feed = column()
            .id(("feed", self.current))
            .track_scroll(&chat.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .items_center()
            .px(px(pad));
        if chat.turns.is_empty() {
            feed = feed.justify_center().child(
                column()
                    .items_center()
                    .gap_2()
                    .child(self.welcome.clone())
                    .child(heading("What would you like to work on?", 25.)),
            );
        } else {
            let latest = chat.turns.iter().rposition(|t| t.reply.is_some());
            let mut turns = column()
                .w_full()
                .max_w(px(758.))
                .pt_6()
                .pb_5()
                .gap_6()
                .flex_none();
            for t in 0..chat.turns.len() {
                turns = turns.child(self.turn(t, latest, compact, cx));
            }
            feed = feed.child(turns);
        }
        let composer = row()
            .gap_2()
            .w_full()
            .px_3()
            .h(px(55.))
            .rounded_xl()
            .bg(rgb(WHITE))
            .border_1()
            .border_color(rgb(0xc5d1c7))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(chat.draft.clone()),
            )
            .child(
                hit("send")
                    .size(px(33.))
                    .justify_center()
                    .rounded_lg()
                    .bg(rgb(GREEN))
                    .hover(|s| s.bg(rgb(0x36755b)))
                    .child(Icon::Arrow.draw(17., WHITE))
                    .on_click(cx.listener(|s, _, _, cx| s.send(cx))),
            );
        let status = if chat.waiting() {
            "Waiting for approval"
        } else {
            "Ready"
        };
        let footer = column()
            .px(px(pad))
            .pt_2()
            .pb_4()
            .items_center()
            .flex_none()
            .child(
                column()
                    .max_w(px(758.))
                    .w_full()
                    .gap_2()
                    .child(composer)
                    .child(
                        row()
                            .px_1()
                            .justify_between()
                            .child(
                                row()
                                    .gap_2()
                                    .child(dot(if chat.waiting() { 0xb18a47 } else { 0x769a7d }))
                                    .child(label(status, 11., MUTED)),
                            )
                            .child(label("Return to send", 11., SOFT)),
                    ),
            );
        column()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(feed)
            .child(footer)
    }
    fn library(&self, cx: &mut Context<Self>) -> Div {
        let mut files = column().gap_4().w_full().max_w(px(720.));
        for (i, chat) in self.chats.iter().enumerate().rev() {
            for (t, turn) in chat.turns.iter().enumerate() {
                if let Some(f) = turn.published() {
                    files = files.child(
                        column()
                            .gap_2()
                            .child(label(chat.title.clone(), 12., MUTED))
                            .child(self.file_link(i, t, f, format!("library-file-{i}-{t}"), cx)),
                    );
                }
            }
        }
        column()
            .flex_1()
            .min_w_0()
            .child(
                row()
                    .h(px(66.))
                    .px_6()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .child(heading("Artifacts", 18.)),
            )
            .child(
                column()
                    .id("library-files")
                    .items_center()
                    .overflow_y_scroll()
                    .p_6()
                    .child(files),
            )
    }
    fn dock(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        let chat = self.chat();
        let active = chat.active.expect("open dock");
        let mut tabs = row()
            .id("dock-tabs")
            .overflow_x_scroll()
            .gap_1()
            .h(px(41.))
            .px_2()
            .border_b_1()
            .border_color(rgb(LINE))
            .flex_none();
        for (index, &tab) in chat.tabs.iter().enumerate() {
            tabs = tabs.child(
                hit(format!("tab-{index}"))
                    .px_2()
                    .h(px(26.))
                    .max_w(px(140.))
                    .flex_none()
                    .bg(rgb(if tab == active { TINT } else { RAIL }))
                    .child(
                        label(
                            chat.tab_title(tab).to_owned(),
                            11.,
                            if tab == active { GREEN } else { MUTED },
                        )
                        .truncate(),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| s.open(tab, cx))),
            );
        }
        let mut content = column()
            .id("dock-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_4()
            .p_4();
        match active {
            Tab::Worker(t, w) => {
                let worker = &chat.turns[t].workers[w];
                content = content
                    .child(row().gap_2().child(tile(Icon::Branch, TINT, GREEN)).child(
                        column().child(heading(worker.name, 17.)).child(label(
                            "Child agent  ·  Finished",
                            11.,
                            MUTED,
                        )),
                    ))
                    .child(
                        column()
                            .gap_2()
                            .mt_2()
                            .child(
                                label("From root agent", 11., MUTED)
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                label(worker.task, 14., INK)
                                    .line_height(px(21.))
                                    .p_3()
                                    .bg(rgb(TINT))
                                    .rounded_lg(),
                            ),
                    )
                    .child(
                        column()
                            .gap_2()
                            .child(heading("Final result", 13.))
                            .child(label(worker.finding, 14., INK).line_height(px(22.))),
                    )
                    .child(
                        row()
                            .gap_2()
                            .py_3()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .child(Icon::Check.draw(14., GREEN))
                            .child(label("Delivered to root agent", 11., MUTED)),
                    );
            }
            Tab::Artifact(t) => {
                let file = chat.turns[t].published().expect("published file");
                content = content
                    .child(
                        row()
                            .gap_2()
                            .child(Icon::File.draw(15., GREEN))
                            .child(label("Markdown  ·  Revision 1", 11., MUTED)),
                    )
                    .child(heading(file.title, 20.));
                for &(title, body) in file.sections {
                    content = content.child(
                        column()
                            .gap_2()
                            .child(heading(title, 14.))
                            .child(label(body, 14., INK).line_height(px(21.))),
                    );
                }
                content = content.child(
                    hit("copy-file")
                        .gap_2()
                        .p_2()
                        .hover(|s| s.bg(rgb(TINT)))
                        .child(Icon::Copy.draw(14., GREEN))
                        .child(label("Copy Markdown", 12., GREEN))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(file.markdown()))
                        }),
                );
            }
            Tab::Workspace => {
                content = content.child(heading("Chat artifacts", 16.));
                let mut count = 0;
                for (t, turn) in chat.turns.iter().enumerate() {
                    if let Some(f) = turn.published() {
                        count += 1;
                        content = content.child(self.file_link(
                            self.current,
                            t,
                            f,
                            format!("workspace-file-{t}"),
                            cx,
                        ));
                    }
                }
                if count == 0 {
                    content = content.child(label("Published files will appear here.", 13., MUTED));
                }
                content = content
                    .child(
                        heading("MCP servers", 15.)
                            .pt_4()
                            .border_t_1()
                            .border_color(rgb(LINE)),
                    )
                    .child(
                        hit("server")
                            .gap_2()
                            .p_2()
                            .border_1()
                            .border_color(rgb(LINE))
                            .hover(|s| s.bg(rgb(TINT)))
                            .child(Icon::Plug.draw(17., GREEN))
                            .child(
                                column()
                                    .flex_1()
                                    .child(heading("Field catalog", 13.))
                                    .child(label("Sample server  ·  2 tools", 11., MUTED)),
                            )
                            .child(
                                if self.show_tools {
                                    Icon::Down
                                } else {
                                    Icon::Right
                                }
                                .draw(13., MUTED),
                            )
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.show_tools = !s.show_tools;
                                cx.notify()
                            })),
                    );
                if self.show_tools {
                    content = content
                        .child(label("list_sensors\nread_station", 13., INK).line_height(px(22.)));
                }
            }
        }
        column()
            .w(px(if compact { 271. } else { 313. }))
            .flex_none()
            .h_full()
            .bg(rgb(0xf6f7f2))
            .border_l_1()
            .border_color(rgb(LINE))
            .child(
                row()
                    .px_4()
                    .justify_between()
                    .h(px(66.))
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .child(heading(
                        match active {
                            Tab::Workspace => "Workspace",
                            Tab::Worker(..) => "Agent conversation",
                            Tab::Artifact(_) => "Artifact preview",
                        },
                        14.,
                    ))
                    .child(
                        hit("close-tab")
                            .size(px(28.))
                            .justify_center()
                            .hover(|s| s.bg(rgb(TINT)))
                            .child(Icon::Close.draw(14., MUTED))
                            .on_click(cx.listener(move |s, _, _, cx| {
                                s.chats[s.current].close(active);
                                cx.notify()
                            })),
                    ),
            )
            .child(tabs)
            .child(content)
    }
}
impl Render for Desktop {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = w.viewport_size().width < px(1080.);
        let open = !self.library && self.chat().active.is_some();
        let mut shell = div()
            .size_full()
            .flex()
            .bg(rgb(PAPER))
            .font_family("Inter")
            .text_color(rgb(INK))
            .on_action(cx.listener(|s, _: &SendMessage, w, cx| {
                if !s.search.focus_handle(cx).is_focused(w) {
                    s.send(cx);
                }
            }))
            .child(self.sidebar(compact, cx));
        if self.library {
            shell = shell.child(self.library(cx));
        } else {
            shell = shell.child(self.conversation(open || compact, cx));
        }
        if open {
            shell = shell.child(self.dock(compact, cx));
        }
        shell
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    fn click(cx: &mut VisualTestContext, id: &'static str) {
        let b = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("missing {id}"));
        cx.simulate_click(b.center(), Default::default());
        cx.run_until_parked();
    }
    fn window(cx: &mut TestAppContext) -> (Entity<Desktop>, &mut VisualTestContext) {
        cx.update(|app| {
            crate::input::bind(app);
            app.bind_keys([KeyBinding::new("enter", SendMessage, Some("TextInput"))]);
        });
        let (view, cx) = cx.add_window_view(|_, cx| Desktop::new(cx));
        cx.simulate_resize(size(px(1180.), px(745.)));
        cx.run_until_parked();
        (view, cx)
    }
    #[gpui::test]
    fn inspect_workers_and_publication(cx: &mut TestAppContext) {
        let (v, cx) = window(cx);
        cx.update(|_, app| assert!(v.read(app).chat().active.is_none()));
        click(cx, "activity-0");
        cx.update(|_, app| assert!(!v.read(app).chat().turns[0].expanded));
        assert!(cx.debug_bounds("approve").is_some());
        click(cx, "activity-0");
        click(cx, "worker-0-0");
        click(cx, "worker-0-1");
        click(cx, "tab-0");
        cx.update(|_, app| assert_eq!(v.read(app).chat().active, Some(Tab::Worker(0, 0))));
        click(cx, "close-tab");
        click(cx, "close-tab");
        cx.update(|_, app| assert!(v.read(app).chat().active.is_none()));
        click(cx, "approve");
        click(cx, "attachment-0");
        click(cx, "workspace");
        click(cx, "server");
        click(cx, "workspace-file-0");
        cx.update(|_, app| {
            let v = v.read(app);
            assert!(v.show_tools);
            assert_eq!(v.chat().tabs.len(), 2);
            assert_eq!(v.chat().active, Some(Tab::Artifact(0)));
            assert_eq!(
                v.chat().turns[0].published().unwrap().filename,
                "field-station-brief.md"
            );
        });
        click(cx, "library");
        click(cx, "library-file-0-0");
        cx.update(|_, app| {
            let v = v.read(app);
            assert_eq!(v.current, 0);
            assert_eq!(
                v.chat().tab_title(v.chat().active.unwrap()),
                "field-trial-checklist.md"
            );
        });
    }
    #[gpui::test]
    fn draft_search_and_new_chat(cx: &mut TestAppContext) {
        let (v, cx) = window(cx);
        click(cx, "decline");
        assert!(cx.debug_bounds("attachment-0").is_none());
        click(cx, "new");
        cx.simulate_input("Which station?");
        click(cx, "search");
        cx.simulate_input("Deployment");
        cx.run_until_parked();
        cx.update(|w, app| {
            let v = v.read(app);
            assert!(v.searching);
            assert!(v.search.focus_handle(app).is_focused(w));
            assert_eq!(v.search.read(app).text(), "Deployment");
        });
        assert!(cx.debug_bounds("chat-0").is_some());
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| assert!(v.read(app).chats[2].turns.is_empty()));
        click(cx, "history");
        click(cx, "chat-2");
        click(cx, "send");
        click(cx, "new");
        cx.simulate_input("Second question");
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            let v = v.read(app);
            assert_eq!(v.chats[2].turns[0].prompt, "Which station?");
            assert_eq!(v.chat().turns[0].prompt, "Second question");
            assert!(v.chat().draft.read(app).text().is_empty());
        });
    }
    #[gpui::test]
    fn narrow_window_keeps_chat_and_dock_visible(cx: &mut TestAppContext) {
        let (_v, cx) = window(cx);
        cx.simulate_resize(size(px(850.), px(560.)));
        cx.run_until_parked();
        click(cx, "workspace");
        let send = cx.debug_bounds("send").unwrap();
        let close = cx.debug_bounds("close-tab").unwrap();
        assert!(send.right() < close.left());
        assert!(close.right() <= px(850.));
        click(cx, "close-tab");
        assert!(cx.debug_bounds("send").unwrap().right() > send.right());
    }
}
