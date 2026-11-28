//! Chat commands and presentation for the selected daemon.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Draft {
    pub(super) input: Entity<TextInput>,
    sending: bool,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct Conversation {
    drafts: BTreeMap<(String, Uuid), Draft>,
    decisions: BTreeSet<(String, String)>,
    collapsed: BTreeSet<i64>,
    show_events: bool,
    loaded: bool,
    scroll: gpui::ScrollHandle,
}
impl Conversation {
    pub(super) fn reset_view(&mut self) {
        self.collapsed.clear();
        self.show_events = false;
        self.loaded = false;
        self.scroll = gpui::ScrollHandle::new();
    }
}

impl Servers {
    fn draft_key(&self) -> Option<(String, Uuid)> {
        Some((self.active.url.clone(), self.chat?))
    }
    pub(super) fn current_draft(&self) -> Option<&Draft> {
        self.conversation.drafts.get(&self.draft_key()?)
    }
    pub(super) fn ensure_draft(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key() {
            self.conversation
                .drafts
                .entry(key)
                .or_insert_with(|| Draft {
                    input: cx.new(TextInput::new),
                    sending: false,
                    error: None,
                });
        }
    }
    pub(super) fn remember_chat(&mut self, child: Option<i64>) {
        let Some(chat_id) = self.chat else { return };
        let location = ChatLocation {
            chat_id,
            inspected_child: child,
        };
        if self.profiles.locations.get(&self.active.url) == Some(&location) {
            return;
        }
        let mut next = self.profiles.clone();
        next.locations.insert(self.active.url.clone(), location);
        if let Err(e) = self.save_profiles(next) {
            self.config_error = Some(e.to_string());
        }
    }
    fn inspect(&mut self, id: Option<i64>, cx: &mut Context<Self>) {
        let child = id.filter(|id| {
            self.runs
                .iter()
                .any(|r| r.run_id == *id && r.parent_agent_id.is_some())
        });
        self.remember_chat(child);
        self.follow_run(id, cx);
    }
    fn follow_run(&mut self, id: Option<i64>, cx: &mut Context<Self>) {
        if self.run != id {
            self.run = id;
            self.events.clear();
            self.conversation.show_events = false;
            self.watch(cx);
        }
        cx.notify();
    }
    fn latest_root(&self) -> Option<i64> {
        self.runs
            .iter()
            .rev()
            .find(|r| r.parent_agent_id.is_none())
            .map(|r| r.run_id)
    }
    fn pending_request(&self, server: &str, chat: Uuid) -> Option<Uuid> {
        self.profiles
            .pending_requests
            .get(server)?
            .get(&chat)
            .copied()
    }
    pub(super) fn sync_conversation(&mut self, cx: &mut Context<Self>) {
        let Some((server, chat)) = self.draft_key() else {
            return;
        };
        self.ensure_draft(cx);
        if !self.conversation.loaded {
            self.conversation.scroll.scroll_to_bottom();
        }
        self.conversation.loaded = true;
        if let Some(key) = self.pending_request(&server, chat).map(|id| id.to_string())
            && let Some(accepted) = self.runs.iter().find(|r| r.run_key == key)
        {
            let accepted_id = accepted.run_id;
            if let Some(draft) = self.conversation.drafts.get_mut(&(server.clone(), chat)) {
                if draft.input.read(cx).text().trim() == accepted.task {
                    draft.input.update(cx, |input, cx| {
                        input.reset();
                        cx.notify();
                    });
                }
                draft.sending = false;
                draft.error = None;
            }
            let mut next = self.profiles.clone();
            if let Some(pending) = next.pending_requests.get_mut(&server) {
                pending.remove(&chat);
                if pending.is_empty() {
                    next.pending_requests.remove(&server);
                }
            }
            if let Err(e) = self.save_profiles(next) {
                self.config_error = Some(e.to_string());
            }
            if self
                .profiles
                .locations
                .get(&server)
                .and_then(|l| l.inspected_child)
                .is_none()
            {
                self.follow_run(Some(accepted_id), cx);
            }
        }
        if self
            .run
            .is_none_or(|id| !self.runs.iter().any(|r| r.run_id == id))
        {
            let child = self
                .profiles
                .locations
                .get(&server)
                .and_then(|l| l.inspected_child)
                .filter(|id| {
                    self.runs
                        .iter()
                        .any(|r| r.run_id == *id && r.parent_agent_id.is_some())
                });
            self.follow_run(child.or_else(|| self.latest_root()), cx);
        }
    }
    fn busy(&self) -> bool {
        let root = self
            .chats
            .iter()
            .find(|c| Some(c.chat_id) == self.chat)
            .map(|c| c.root_agent_id)
            .or_else(|| {
                self.runs
                    .iter()
                    .rev()
                    .find(|r| r.parent_agent_id.is_none())
                    .map(|r| r.agent_id)
            });
        self.runs
            .iter()
            .any(|r| Some(r.agent_id) == root && !r.finished())
    }
    pub(super) fn can_send(&self) -> bool {
        self.form.is_none()
            && matches!(self.status, Status::Connected)
            && self.conversation.loaded
            && !self.busy()
            && self.current_draft().is_some_and(|d| !d.sending)
    }
    pub(super) fn send_prompt(&mut self, cx: &mut Context<Self>) {
        if !self.can_send() {
            return;
        }
        let Some((url, chat)) = self.draft_key() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let prompt = self
            .current_draft()
            .expect("selected chat has a draft")
            .input
            .read(cx)
            .text()
            .trim()
            .to_string();
        if prompt.is_empty() {
            return;
        }
        let request = match self.pending_request(&url, chat) {
            Some(request) => request,
            None => {
                let request = Uuid::new_v4();
                let mut next = self.profiles.clone();
                next.pending_requests
                    .entry(url.clone())
                    .or_default()
                    .insert(chat, request);
                if let Err(e) = self.save_profiles(next) {
                    self.config_error = Some(e.to_string());
                    cx.notify();
                    return;
                }
                request
            }
        };
        let inspected_at_send = self.run;
        let key = (url, chat);
        let draft = self
            .conversation
            .drafts
            .get_mut(&key)
            .expect("selected chat has a draft");
        draft.sending = true;
        draft.error = None;
        self.conversation.scroll.scroll_to_bottom();
        let post = self
            .runtime
            .spawn(async move { client.send_prompt(chat, request, &prompt).await });
        cx.spawn(async move |view, cx| {
            let result = post.await.map_err(anyhow::Error::from).and_then(|v| v);
            let _ = view.update(cx, |s, cx| {
                // A snapshot can confirm this submission before the POST returns.
                if s.pending_request(&key.0, chat) != Some(request) {
                    return;
                }
                let visible = s.draft_key().as_ref() == Some(&key);
                let draft = s
                    .conversation
                    .drafts
                    .get_mut(&key)
                    .expect("submitted draft exists");
                match result {
                    Ok(run) if visible && s.run == inspected_at_send => {
                        s.run = Some(run);
                        s.events.clear();
                        s.remember_chat(None);
                        s.watch(cx);
                    }
                    Ok(_) => {
                        draft.sending = false;
                    }
                    Err(e) => {
                        draft.sending = false;
                        draft.error = Some(format!("{e:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn decide(&mut self, id: String, approved: bool, cx: &mut Context<Self>) {
        let Some(key) = self.draft_key() else { return };
        let Some(client) = self.client.clone() else {
            return;
        };
        if !matches!(self.status, Status::Connected)
            || !self.approvals.iter().any(|a| {
                a.wait["id"].as_str() == Some(&id)
                    && self
                        .runs
                        .iter()
                        .any(|r| r.agent_id == a.agent_id && r.status == "waiting")
            })
        {
            return;
        }
        let decision = (key.0.clone(), id.clone());
        if !self.conversation.decisions.insert(decision.clone()) {
            return;
        }
        let post = self
            .runtime
            .spawn(async move { client.decide_approval(&id, approved).await });
        cx.spawn(async move |view, cx| {
            let result = post.await.map_err(anyhow::Error::from).and_then(|v| v);
            let _ = view.update(cx, |s, cx| {
                s.conversation.decisions.remove(&decision);
                if let Err(e) = &result
                    && let Some(draft) = s.conversation.drafts.get_mut(&key)
                {
                    draft.error = Some(format!("{e:#}"));
                }
                if s.draft_key().as_ref() == Some(&key) {
                    if result.is_ok() {
                        s.approvals
                            .retain(|a| a.wait["id"].as_str() != Some(&decision.1));
                    }
                    s.watch(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn approval_card(&self, approval: &Approval, cx: &mut Context<Self>) -> Option<Div> {
        let id = approval.wait["id"].as_str()?;
        let tool = &approval.wait["on"]["ToolApproval"];
        let name = tool["tool_name"].as_str()?;
        let args = serde_json::to_string_pretty(tool.get("arguments")?).ok()?;
        let pending = self
            .conversation
            .decisions
            .contains(&(self.active.url.clone(), id.to_string()));
        let mut card = column()
            .p_3()
            .gap_2()
            .border_1()
            .border_color(rgb(0xe6d8b9))
            .rounded_lg()
            .bg(rgb(0xfaf6ec))
            .child(
                row()
                    .gap_2()
                    .child(Icon::Gate.draw(16., AMBER))
                    .child(heading(
                        format!("Agent {} requests approval", approval.agent_id),
                        14.,
                    )),
            )
            .child(heading(name.to_string(), 13.))
            .child(label(args, 13., INK));
        if pending {
            card = card.child(label("Sending decision…", 12., MUTED));
        } else if matches!(self.status, Status::Connected) {
            let mut buttons = row().gap_2();
            for (approved, text) in [(true, "Approve once"), (false, "Decline")] {
                let id = id.to_string();
                buttons = buttons.child(
                    hit(format!(
                        "{}-{id}",
                        if approved { "approve" } else { "decline" }
                    ))
                    .px_3()
                    .py_1()
                    .bg(rgb(if approved { GREEN } else { WHITE }))
                    .child(label(text, 13., if approved { WHITE } else { INK }))
                    .on_click(cx.listener(move |s, _, _, cx| s.decide(id.clone(), approved, cx))),
                );
            }
            card = card.child(buttons);
        }
        Some(card)
    }
    fn event_list(&self, run: i64, cx: &mut Context<Self>) -> Div {
        let active = self.run == Some(run) && self.conversation.show_events;
        let mut list = column().gap_1().child(
            hit(format!("events-{run}"))
                .py_1()
                .gap_2()
                .child(if active { Icon::Down } else { Icon::Right }.draw(12., MUTED))
                .child(label("Run events", 12., MUTED))
                .on_click(cx.listener(move |s, _, _, cx| {
                    s.inspect(Some(run), cx);
                    s.conversation.show_events = !active;
                    cx.notify();
                })),
        );
        if active {
            let mut entries = column()
                .id(("event-scroll", run as u64))
                .max_h(px(150.))
                .overflow_y_scroll()
                .p_2()
                .gap_1()
                .bg(rgb(RAIL));
            for e in self.events.iter().rev() {
                entries = entries.child(label(
                    format!("{}  ·  {}", e.sequence, e.event.kind.replace('_', " ")),
                    12.,
                    MUTED,
                ));
            }
            list = list.child(entries);
        }
        list
    }
    fn turn(
        &self,
        root: &Run,
        children: &[&Run],
        latest: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = root.run_id;
        let expanded = !self.conversation.collapsed.contains(&id);
        let count = children.len();
        let mut body = column()
            .gap_2()
            .flex_1()
            .min_w_0()
            .child(heading("Wakefold", 14.))
            .child(
                hit(format!("activity-{id}"))
                    .gap_2()
                    .py_1()
                    .child(if expanded { Icon::Down } else { Icon::Right }.draw(13., MUTED))
                    .child(heading("Activity", 12.))
                    .child(label(
                        match count {
                            0 => root.status.clone(),
                            1 => "1 child agent".into(),
                            n => format!("{n} child agents"),
                        },
                        12.,
                        MUTED,
                    ))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if !s.conversation.collapsed.remove(&id) {
                            s.conversation.collapsed.insert(id);
                        }
                        cx.notify()
                    })),
            );
        if expanded {
            let mut cards = row().flex_wrap().gap_2();
            for r in children {
                let child_id = r.run_id;
                cards = cards.child(
                    hit(format!("child-{child_id}"))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(LINE))
                        .bg(rgb(if self.run == Some(child_id) {
                            TINT
                        } else {
                            WHITE
                        }))
                        .hover(|s| s.bg(rgb(TINT)))
                        .child(tile(Icon::Branch, TINT, GREEN))
                        .child(
                            column()
                                .child(heading(format!("Agent {}", r.agent_id), 12.))
                                .child(label(r.status.clone(), 11., MUTED)),
                        )
                        .child(Icon::Right.draw(12., MUTED))
                        .on_click(cx.listener(move |s, _, _, cx| s.inspect(Some(child_id), cx))),
                );
            }
            if count > 0 {
                body = body.child(cards);
            }
            body = body.child(self.event_list(id, cx));
        }
        if let Some(outcome) = &root.outcome {
            if root.status == "failed" {
                body = body.child(heading("Run failed", 13.));
            }
            body = body.child(label(outcome.clone(), 15., INK).line_height(px(23.)));
        } else {
            body = body.child(label(
                format!("{} · {}", root.status, root.latest_event.replace('_', " ")),
                12.,
                MUTED,
            ));
        }
        for a in &self.approvals {
            if std::iter::once(root)
                .chain(children.iter().copied())
                .any(|r| r.status == "waiting" && r.agent_id == a.agent_id)
                && let Some(card) = self.approval_card(a, cx)
            {
                body = body.child(card);
            }
        }
        let figure = if Some(id) == latest {
            self.portrait.clone().into_any_element()
        } else {
            self.character.read(cx).still(54., 46.).into_any_element()
        };
        column()
            .gap_4()
            .flex_none()
            .child(
                row().justify_end().child(
                    label(root.task.clone(), 15., INK)
                        .max_w(px(550.))
                        .rounded_xl()
                        .p_3()
                        .bg(rgb(TINT))
                        .line_height(px(22.)),
                ),
            )
            .child(div().flex().items_start().gap_2().child(figure).child(body))
    }
    fn child_panel(&self, child: &Run, cx: &mut Context<Self>) -> Div {
        let mut panel = column()
            .id("child-panel")
            .debug_selector(|| "child-panel".into())
            .w(px(274.))
            .flex_none()
            .h_full()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .bg(rgb(RAIL))
            .border_l_1()
            .border_color(rgb(LINE))
            .child(
                row()
                    .justify_between()
                    .child(heading(format!("Child agent {}", child.agent_id), 16.))
                    .child(
                        hit("close-child")
                            .p_2()
                            .child(Icon::Close.draw(15., MUTED))
                            .on_click(cx.listener(|s, _, _, cx| s.inspect(s.latest_root(), cx))),
                    ),
            )
            .child(label(child.status.clone(), 12., MUTED))
            .child(heading("Assignment", 13.))
            .child(label(child.task.clone(), 14., INK).line_height(px(21.)));
        if let Some(outcome) = &child.outcome {
            panel = panel
                .child(heading(
                    if child.status == "failed" {
                        "Failure"
                    } else {
                        "Final result"
                    },
                    13.,
                ))
                .child(label(outcome.clone(), 14., INK).line_height(px(21.)));
        }
        column()
            .h_full()
            .child(panel.child(self.event_list(child.run_id, cx)))
    }
    pub(super) fn chat_view(&self, cx: &mut Context<Self>) -> Div {
        let title = self
            .chats
            .iter()
            .find(|c| Some(c.chat_id) == self.chat)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| "New chat".into());
        let mut feed = column()
            .id("conversation")
            .track_scroll(&self.conversation.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_5()
            .py_4()
            .items_center()
            .gap_5();
        if self.runs.is_empty() {
            feed = feed
                .justify_center()
                .child(self.character.clone())
                .child(heading(
                    if self.conversation.loaded {
                        "What would you like to work on?"
                    } else {
                        "Opening chat…"
                    },
                    20.,
                ));
        } else {
            let mut children = BTreeMap::<i64, Vec<&Run>>::new();
            for run in &self.runs {
                if let Some(parent) = run.spawned_by_run_id {
                    children.entry(parent).or_default().push(run);
                }
            }
            let latest = self.latest_root();
            for root in self.runs.iter().filter(|r| r.parent_agent_id.is_none()) {
                feed = feed.child(
                    column().w_full().max_w(px(758.)).flex_none().child(
                        self.turn(
                            root,
                            children
                                .get(&root.run_id)
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                            latest,
                            cx,
                        ),
                    ),
                );
            }
        }
        let mut main = column()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(heading(title, 16.).truncate().px_5().py_3())
            .child(feed);
        if let Some(draft) = self.current_draft() {
            let status = if draft.sending {
                "Sending…"
            } else if self.busy() {
                "Run in progress"
            } else if !self.conversation.loaded {
                "Loading chat…"
            } else if self
                .chat
                .is_some_and(|chat| self.pending_request(&self.active.url, chat).is_some())
            {
                "Previous send unconfirmed. Send the same message to retry."
            } else {
                ""
            };
            let mut footer = column().flex_none().px_5().pb_4().gap_1();
            if let Some(error) = self.config_error.as_ref().or(draft.error.as_ref()) {
                footer = footer.child(label(error.clone(), 12., AMBER));
            }
            if let Status::Reconnecting(e) | Status::Attention(e) = &self.status {
                footer = footer.child(label(e.clone(), 12., AMBER));
            }
            if !status.is_empty() {
                footer = footer.child(label(status, 12., MUTED));
            }
            let mut send = hit("send")
                .size(px(33.))
                .justify_center()
                .bg(rgb(if self.can_send() { GREEN } else { MUTED }))
                .child(Icon::Arrow.draw(17., WHITE));
            if self.can_send() {
                send = send.on_click(cx.listener(|s, _, _, cx| s.send_prompt(cx)));
            }
            main = main.child(
                footer.child(
                    row()
                        .gap_2()
                        .p_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(LINE))
                        .bg(rgb(WHITE))
                        .child(
                            div()
                                .id("composer")
                                .debug_selector(|| "composer".into())
                                .flex_1()
                                .min_w_0()
                                .child(draft.input.clone()),
                        )
                        .child(send),
                ),
            );
        }
        let mut layout = div().flex().flex_1().min_h_0().child(main);
        if let Some(child) = self
            .runs
            .iter()
            .find(|r| Some(r.run_id) == self.run && r.parent_agent_id.is_some())
        {
            layout = layout.child(self.child_panel(child, cx));
        }
        layout
    }
}
