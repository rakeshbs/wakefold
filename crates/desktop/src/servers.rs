//! Server selection, saved chats and the currently watched run.
mod artifacts;
mod conversation;
mod dock;
mod mcp;
#[cfg(test)]
mod workflow_test;
use crate::{
    avatar::{Artwork, Character},
    connection::{Approval, Daemon, Event, Run, SavedChat, Update},
    icons::Icon,
    input::TextInput,
    local_daemon,
    profiles::{ChatLocation, LOCAL, Profile, Profiles},
    style::*,
};
use anyhow::{Context as _, Result};
use conversation::Conversation;
use dock::{Dock, Panel};
use gpui::{Context, Div, Entity, Focusable, Render, Task, Window, div, prelude::*, px, rgb};
use std::{collections::VecDeque, path::PathBuf};
use tokio::{runtime::Runtime, sync::mpsc};
use uuid::Uuid;

enum Status {
    Connecting,
    Connected,
    Reconnecting(String),
    Attention(String),
    NeedsLocalKey(String),
}
impl Status {
    fn label(&self) -> &str {
        match self {
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Reconnecting(_) => "Reconnecting",
            Self::Attention(_) => "Needs attention",
            Self::NeedsLocalKey(_) => "Provider key needed",
        }
    }
}
struct AddServer {
    name: Entity<TextInput>,
    url: Entity<TextInput>,
    token: Option<String>,
    saving: bool,
    error: Option<String>,
}
pub(crate) struct Servers {
    runtime: Runtime,
    settings: Option<PathBuf>,
    profiles: Profiles,
    active: Profile,
    client: Option<Daemon>,
    status: Status,
    form: Option<AddServer>,
    config_error: Option<String>,
    load: Option<Task<()>>,
    save: Option<Task<()>>,
    reader: Option<Task<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    chats: Vec<SavedChat>,
    chat: Option<Uuid>,
    runs: Vec<Run>,
    approvals: Vec<Approval>,
    run: Option<i64>,
    events: VecDeque<Event>,
    character: Entity<Character>,
    portrait: Entity<Character>,
    conversation: Conversation,
    dock: Dock,
    viewport: Option<gpui::Size<gpui::Pixels>>,
    sidebar_expanded: Option<bool>,
    watch_version: u64,
    artifacts: artifacts::Artifacts,
    opened_files: Vec<tempfile::TempDir>,
    mcp: Option<mcp::McpView>,
}
impl Servers {
    pub(crate) fn new(runtime: Runtime, cx: &mut Context<Self>) -> Self {
        Self::with_settings(
            runtime,
            local_daemon::config_dir().map(|p| p.join("servers.json")),
            cx,
        )
    }
    fn with_settings(runtime: Runtime, path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let settings = path
            .as_deref()
            .context("Could not locate the application settings directory.")
            .and_then(Profiles::load);
        let (profiles, config_error) = match settings {
            Ok(p) => (p, None),
            Err(e) => (Profiles::default(), Some(format!("{e:#}"))),
        };
        let active = profiles
            .servers
            .iter()
            .find(|p| Some(&p.url) == profiles.selected.as_ref())
            .cloned()
            .unwrap_or_else(Profile::local);
        let art = Artwork::load();
        let portrait = cx.new(|_| Character::new(art.clone(), (54., 46.)));
        let character = cx.new(|_| Character::new(art, (180., 140.)));
        let mut view = Self {
            runtime,
            // Preserve a settings file that could not be read.
            settings: path.filter(|_| config_error.is_none()),
            profiles,
            active,
            client: None,
            status: Status::Connecting,
            form: None,
            config_error,
            load: None,
            save: None,
            reader: None,
            worker: None,
            chats: vec![],
            chat: None,
            runs: vec![],
            approvals: vec![],
            run: None,
            events: VecDeque::new(),
            character,
            portrait,
            conversation: Conversation::default(),
            dock: Dock::default(),
            viewport: None,
            sidebar_expanded: None,
            watch_version: 0,
            artifacts: Default::default(),
            opened_files: Vec::new(),
            mcp: None,
        };
        view.connect(view.active.clone(), cx);
        view
    }
    fn remember(&mut self, profile: Profile) -> Result<()> {
        if self.profiles.selected.as_deref() == Some(&profile.url)
            && (profile.url == LOCAL
                || self
                    .profiles
                    .servers
                    .iter()
                    .any(|p| p.name == profile.name && p.url == profile.url))
        {
            return Ok(());
        }
        let mut next = self.profiles.clone();
        next.remember(profile);
        self.save_profiles(next)
    }
    fn save_profiles(&mut self, next: Profiles) -> Result<()> {
        next.save(self.settings.as_deref().with_context(|| {
            self.config_error
                .clone()
                .unwrap_or_else(|| "Could not locate the settings file.".into())
        })?)?;
        self.profiles = next;
        self.config_error = None;
        Ok(())
    }
    fn stop_reader(&mut self) {
        self.watch_version += 1;
        self.reader = None;
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
    fn connect(&mut self, profile: Profile, cx: &mut Context<Self>) {
        self.stop_reader();
        self.load = None;
        self.save = None;
        self.client = None;
        self.form = None;
        self.artifacts = Default::default();
        self.mcp = None;
        self.chats.clear();
        self.runs.clear();
        self.events.clear();
        self.approvals.clear();
        self.chat = self.profiles.locations.get(&profile.url).map(|l| l.chat_id);
        self.run = None;
        self.conversation.reset_view();
        self.dock.clear();
        self.active = profile.clone();
        self.ensure_draft(cx);
        self.status = Status::Connecting;
        let directory = self
            .settings
            .as_ref()
            .and_then(|path| path.parent())
            .map(|p| p.to_owned());
        let runtime = self.runtime.handle().clone();
        self.load = Some(cx.spawn(async move |view, cx| {
            let result = async {
                if profile.url == LOCAL {
                    return local_daemon::connect(runtime, directory.as_deref(), cx).await;
                }
                let (_, token) = cx
                    .update(|app| app.read_credentials(&profile.credential_key()))?
                    .await?
                    .context("Add the server again to save its access token.")?;
                Self::client(&profile, Some(std::str::from_utf8(&token)?)).map(Some)
            }
            .await;
            let _ = view.update(cx, |s, cx| {
                match result {
                    Ok(Some(client)) => {
                        s.client = Some(client);
                        s.watch(cx);
                        if let Err(e) = s.remember(profile) {
                            s.config_error = Some(format!("{e:#}"));
                        }
                    }
                    Ok(None) => s.status = Status::NeedsLocalKey(String::new()),
                    Err(e) => s.status = Status::Attention(format!("{e:#}")),
                }
                cx.notify()
            });
        }));
        cx.notify()
    }
    fn save_local_key(&mut self, cx: &mut Context<Self>) {
        let Some(key) = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(|s| s.trim().to_owned())
            .filter(|key| !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()))
        else {
            self.status = Status::NeedsLocalKey("Copy your OpenAI API key and try again.".into());
            cx.notify();
            return;
        };
        let save =
            cx.write_credentials(local_daemon::PROVIDER_CREDENTIAL, "OpenAI", key.as_bytes());
        self.status = Status::Connecting;
        self.load = Some(cx.spawn(async move |view, cx| {
            let result = save.await;
            let _ = view.update(cx, |s, cx| {
                match result {
                    Ok(()) => s.connect(Profile::local(), cx),
                    Err(e) => {
                        s.status = Status::NeedsLocalKey(format!("Could not save the key: {e}"))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn client(profile: &Profile, token: Option<&str>) -> Result<Daemon> {
        let ca = std::env::var_os("WAKEFOLD_CA_FILE").map(PathBuf::from);
        Daemon::new(profile, token, ca.as_deref())
    }
    fn watch(&mut self, cx: &mut Context<Self>) {
        self.stop_reader();
        let Some(client) = self.client.clone() else {
            return;
        };
        let (chat, run) = (self.chat, self.run);
        let version = self.watch_version;
        let (tx, mut rx) = mpsc::channel(64);
        self.worker = Some(
            self.runtime
                .spawn(async move { client.watch(chat, run, tx).await }),
        );
        self.reader = Some(cx.spawn(async move |view, cx| {
            while let Some(update) = rx.recv().await {
                if view
                    .update(cx, |s, cx| {
                        if s.watch_version != version {
                            return;
                        }
                        s.apply(update, cx);
                        cx.notify()
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Snapshot {
                chats,
                runs,
                approvals,
                artifacts,
            } => {
                if self.runs != runs
                    || self.approvals != approvals
                    || self.artifacts.revisions != artifacts
                {
                    self.conversation.follow_if_near_bottom();
                    self.dock.follow_child_if_near_bottom();
                }
                self.chats = chats;
                self.runs = runs;
                self.approvals = approvals;
                self.artifacts.revisions = artifacts;
                let reconnected = !matches!(self.status, Status::Connected);
                self.status = Status::Connected;
                if reconnected && self.mcp.as_ref().is_some_and(|m| m.editor.is_none()) {
                    self.reload_mcp(cx);
                }
                self.sync_conversation(cx);
            }
            Update::Event(e) => {
                // An explicit refresh can replay events already drawn for this run.
                if self.run != Some(e.run_id)
                    || self
                        .events
                        .back()
                        .is_some_and(|last| e.sequence <= last.sequence)
                {
                    return;
                }
                if self.events.len() >= 200 {
                    self.events.pop_front();
                }
                if let Some(run) = self.runs.iter_mut().find(|r| r.run_id == e.run_id)
                    && !run.finished()
                {
                    run.latest_event.clone_from(&e.event.kind);
                    if matches!(e.event.kind.as_str(), "run_finished" | "run_failed") {
                        run.status = if e.event.kind == "run_finished" {
                            "completed"
                        } else {
                            "failed"
                        }
                        .into();
                        run.outcome = e.event.data.as_str().map(str::to_owned);
                    }
                }
                self.conversation.follow_if_near_bottom();
                self.dock.follow_child_if_near_bottom();
                self.dock.receive_event(&e);
                self.events.push_back(e);
            }
            Update::Reconnecting(e) => self.status = Status::Reconnecting(e),
            Update::Attention(e) => self.status = Status::Attention(e),
        }
    }
    fn select_chat(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.mcp = None;

        if self.chat != Some(id) {
            self.artifacts = Default::default();
            self.dock.clear();
            self.chat = Some(id);
            self.run = None;
            self.runs.clear();
            self.events.clear();
            self.conversation.reset_view();
            self.remember_chat(None);
            self.watch(cx);
        }
        self.ensure_draft(cx);
        self.form = None;
        self.save = None;
        cx.notify();
    }
    fn show_form(&mut self, cx: &mut Context<Self>) {
        self.mcp = None;
        self.save = None;
        self.form = Some(AddServer {
            name: cx.new(|c| TextInput::with_placeholder("Home Pi", c)),
            url: cx.new(|c| TextInput::with_placeholder("https://pi.example.net:8700", c)),
            token: None,
            saving: false,
            error: None,
        });
        cx.notify()
    }
    fn save_server(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else { return };
        if form.saving {
            return;
        }
        let profile = match Profile::remote(form.name.read(cx).text(), form.url.read(cx).text()) {
            Ok(p) => p,
            Err(e) => {
                form.error = Some(e.to_string());
                cx.notify();
                return;
            }
        };
        let Some(token) = form.token.clone() else {
            form.error = Some("Paste the daemon access token.".into());
            cx.notify();
            return;
        };
        form.saving = true;
        form.error = None;
        cx.notify();
        let requested = profile.clone();
        let verify = self.runtime.spawn(async move {
            let client = Self::client(&requested, Some(&token))?;
            client.chats().await?;
            Ok::<_, anyhow::Error>(token)
        });
        self.save = Some(cx.spawn(async move |view, cx| {
            let result = async {
                let token = verify.await??;
                let store = cx.update(|app| {
                    app.write_credentials(&profile.credential_key(), "daemon", token.as_bytes())
                })?;
                store.await?;
                view.update(cx, |s, _| s.remember(profile.clone()))??;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = view.update(cx, |s, cx| match result {
                Ok(()) => s.connect(profile, cx),
                Err(e) => {
                    if let Some(form) = &mut s.form {
                        form.saving = false;
                        form.error = Some(format!("{e:#}"));
                    }
                    cx.notify()
                }
            });
        }));
    }
    fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_chat(Uuid::new_v4(), cx);
        if let Some(draft) = self.current_draft() {
            window.focus(&draft.input.focus_handle(cx));
        }
    }
    fn sidebar(&self, expanded: bool, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        if !expanded {
            return column()
                .w(px(68.))
                .h_full()
                .flex_none()
                .items_center()
                .gap_3()
                .py_4()
                .bg(rgb(colors.sidebar))
                .border_r_1()
                .border_color(rgb(colors.border))
                .child(
                    hit("expand-sidebar")
                        .p_2()
                        .child(tile(Icon::Fold, colors.tint, colors.accent))
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.sidebar_expanded = Some(true);
                            cx.notify();
                        })),
                )
                .child(
                    hit("new-chat")
                        .p_2()
                        .child(Icon::New.draw(18., colors.accent))
                        .on_click(cx.listener(|s, _, w, cx| s.new_chat(w, cx))),
                )
                .child(
                    hit("show-history")
                        .p_2()
                        .child(Icon::History.draw(18., colors.accent))
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.sidebar_expanded = Some(true);
                            cx.notify();
                        })),
                )
                .child(
                    hit("mcp-servers")
                        .p_2()
                        .child(Icon::Plug.draw(18., colors.accent))
                        .on_click(cx.listener(|s, _, _, cx| s.show_mcp(cx))),
                )
                .child(div().flex_1())
                .child(crate::theme::toggle_button(false, cx));
        }
        let mut side = column()
            .w(px(260.))
            .flex_none()
            .h_full()
            .p_3()
            .gap_1()
            .bg(rgb(colors.sidebar))
            .border_r_1()
            .border_color(rgb(colors.border))
            .child(
                row()
                    .gap_2()
                    .h(px(52.))
                    .child(tile(Icon::Fold, colors.tint, colors.accent))
                    .child(heading(colors, "Wakefold", 21.).flex_1())
                    .child(
                        hit("collapse-sidebar")
                            .p_2()
                            .child(Icon::Sidebar.draw(16., colors.muted))
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.sidebar_expanded = Some(false);
                                cx.notify();
                            })),
                    ),
            )
            .child(label("Servers", 12., colors.muted).mt_3().mb_1());
        let mut servers = column()
            .id("server-list")
            .max_h(px(175.))
            .overflow_y_scroll();
        for (i, p) in std::iter::once(Profile::local())
            .chain(self.profiles.servers.clone())
            .enumerate()
        {
            servers = servers.child(
                hit(format!("profile-{i}"))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(if self.active.url == p.url {
                        colors.tint
                    } else {
                        colors.sidebar
                    }))
                    .hover(move |s| s.bg(rgb(colors.tint)))
                    .child(Icon::Plug.draw(15., colors.accent))
                    .child(label(p.name.clone(), 13., colors.text).truncate())
                    .on_click(cx.listener(move |s, _, _, cx| s.connect(p.clone(), cx))),
            );
        }
        side = side
            .child(servers)
            .child(
                nav(
                    colors,
                    "add-server",
                    Icon::New,
                    "Add server",
                    self.form.is_some(),
                )
                .on_click(cx.listener(|s, _, _, cx| s.show_form(cx))),
            )
            .child(
                nav(colors, "new-chat", Icon::New, "New chat", false)
                    .mt_3()
                    .on_click(cx.listener(|s, _, w, cx| s.new_chat(w, cx))),
            )
            .child(
                nav(
                    colors,
                    "mcp-servers",
                    Icon::Plug,
                    "MCP servers",
                    self.mcp.is_some(),
                )
                .on_click(cx.listener(|s, _, _, cx| s.show_mcp(cx))),
            )
            .child(label("Saved chats", 12., colors.muted).mt_3().mb_1());
        let mut chats = column()
            .id("saved-chats")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for c in &self.chats {
            let id = c.chat_id;
            chats = chats.child(
                hit(format!("chat-{id}"))
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .bg(rgb(if self.chat == Some(id) {
                        colors.tint
                    } else {
                        colors.sidebar
                    }))
                    .hover(move |s| s.bg(rgb(colors.tint)))
                    .child(dot(colors.accent))
                    .child(
                        label(
                            c.title.as_deref().unwrap_or("Empty chat").to_owned(),
                            13.,
                            colors.text,
                        )
                        .truncate(),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| s.select_chat(id, cx))),
            );
        }
        side.child(chats)
            .child(crate::theme::toggle_button(true, cx))
    }
    fn form(&self, form: &AddServer, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut card =
            column()
                .gap_3()
                .max_w(px(480.))
                .w_full()
                .child(heading(colors, "Add server", 23.));
        for (title, field) in [("Name", &form.name), ("Server URL", &form.url)] {
            card = card.child(
                column()
                    .gap_1()
                    .child(label(title, 12., colors.muted))
                    .child(
                        div()
                            .p_1()
                            .bg(rgb(colors.surface))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(colors.border))
                            .child(field.clone()),
                    ),
            );
        }
        card = card.child(label("Access token", 12., colors.muted)).child(
            row()
                .gap_2()
                .child(
                    action(colors, "paste-token", "Paste token", false).on_click(cx.listener(
                        |s, _, _, cx| {
                            if let Some(f) = &mut s.form
                                && !f.saving
                            {
                                f.token = cx
                                    .read_from_clipboard()
                                    .and_then(|c| c.text())
                                    .map(|s| s.trim().to_owned())
                                    .filter(|s| !s.is_empty());
                                f.error = f
                                    .token
                                    .is_none()
                                    .then(|| "Copy the access token, then paste it here.".into());
                                cx.notify();
                            }
                        },
                    )),
                )
                .child(label(
                    if form.token.is_some() {
                        "Token supplied"
                    } else {
                        "Saved to your OS credential store"
                    },
                    12.,
                    colors.muted,
                )),
        );
        if let Some(e) = &form.error {
            card = card.child(label(e.clone(), 13., colors.warning));
        }
        card.child(
            row()
                .gap_2()
                .child(
                    action(
                        colors,
                        "save-server",
                        if form.saving {
                            "Connecting…"
                        } else {
                            "Connect and save"
                        },
                        true,
                    )
                    .on_click(cx.listener(|s, _, _, cx| s.save_server(cx))),
                )
                .child(
                    action(colors, "cancel", "Cancel", false).on_click(cx.listener(
                        |s, _, _, cx| {
                            s.save = None;
                            s.form = None;
                            cx.notify()
                        },
                    )),
                ),
        )
    }
    fn content(&self, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut body = column()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_5()
            .gap_3()
            .id("run-list")
            .overflow_y_scroll();
        if let Some(form) = &self.form {
            return column().flex_1().child(body.child(self.form(form, cx)));
        }
        if let Status::NeedsLocalKey(error) = &self.status {
            let mut setup = column()
                .gap_3()
                .max_w(px(480.))
                .child(heading(colors, "Start the local daemon", 20.))
                .child(label(
                    "Copy your OpenAI API key, then save it to this computer’s credential store.",
                    14.,
                    colors.text,
                ))
                .child(
                    row().child(
                        action(colors, "local-provider-key", "Paste key and start", true)
                            .on_click(cx.listener(|s, _, _, cx| s.save_local_key(cx))),
                    ),
                );
            if !error.is_empty() {
                setup = setup.child(label(error.clone(), 13., colors.warning));
            }
            return column().flex_1().child(body.child(setup));
        }
        if let Some(mcp) = &self.mcp {
            return self.mcp_view(mcp, cx);
        }
        if self.chat.is_some() {
            return self.chat_view(cx);
        }
        if let Some(error) = &self.config_error {
            body = body.child(label(error.clone(), 13., colors.warning));
        }
        if let Status::Reconnecting(error) | Status::Attention(error) = &self.status {
            body = body.child(label(error.clone(), 13., colors.warning));
        }
        let message = match self.status {
            Status::Connected if self.chats.is_empty() => "Start a new chat",
            Status::Connected => "Choose a saved chat or start a new one",
            Status::Connecting => "Connecting to the daemon…",
            Status::Reconnecting(_) => "Waiting for the daemon",
            Status::Attention(_) => "Check your server connection",
            Status::NeedsLocalKey(_) => "Start the local daemon",
        };
        column().flex_1().child(
            body.child(
                column()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(self.character.clone())
                    .child(heading(colors, message, 20.)),
            ),
        )
    }
}
impl Render for Servers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = crate::theme::palette(cx);
        let viewport = window.viewport_size();
        if self.viewport.replace(viewport) != Some(viewport) {
            self.conversation.follow_if_near_bottom();
            self.dock.follow_child_if_near_bottom();
        }
        let width = f32::from(viewport.width);
        let chat_visible = self.chat.is_some()
            && self.form.is_none()
            && self.mcp.is_none()
            && !matches!(self.status, Status::NeedsLocalKey(_));
        let title = if chat_visible {
            self.chats
                .iter()
                .find(|c| Some(c.chat_id) == self.chat)
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "New chat".into())
        } else {
            self.active.name.clone()
        };
        let mut buttons = row()
            .gap_3()
            .flex_none()
            .child(dot(if matches!(self.status, Status::Connected) {
                colors.accent
            } else {
                colors.warning
            }))
            .child(label(self.status.label().to_owned(), 12., colors.muted));
        if chat_visible {
            buttons = buttons.child(
                action(colors, "chat-artifacts", "Artifacts", false)
                    .on_click(cx.listener(|s, _, _, cx| s.open_panel(Panel::Artifacts, cx))),
            );
        }
        let header = row()
            .justify_between()
            .gap_3()
            .px_5()
            .h(px(64.))
            .flex_none()
            .border_b_1()
            .border_color(rgb(colors.border))
            .child(
                column()
                    .min_w_0()
                    .flex_1()
                    .child(heading(colors, title, 17.).truncate())
                    .child(
                        label(
                            if chat_visible {
                                self.active.name.clone()
                            } else {
                                self.active.url.clone()
                            },
                            12.,
                            colors.muted,
                        )
                        .truncate(),
                    ),
            )
            .child(buttons);
        let main = column()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(header)
            .child(self.content(cx));
        let expanded = self.sidebar_expanded.unwrap_or(width > 860.);
        let workspace = if chat_visible {
            self.split_workspace(main, width - if expanded { 260. } else { 68. }, cx)
        } else {
            main
        };
        row()
            .on_drag_move(crate::scrollbar::drag)
            .on_action(cx.listener(|s, _: &crate::SendMessage, w, cx| {
                if s.mcp.is_none()
                    && s.form.is_none()
                    && s.current_draft()
                        .is_some_and(|d| d.input.focus_handle(cx).is_focused(w))
                {
                    s.send_prompt(cx);
                }
            }))
            .size_full()
            .bg(rgb(colors.background))
            .text_color(rgb(colors.text))
            .font_family("Inter")
            .overflow_hidden()
            .child(self.sidebar(expanded, cx))
            .child(workspace)
    }
}
impl Drop for Servers {
    fn drop(&mut self) {
        self.stop_reader();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};

    #[gpui::test]
    fn changing_servers_discards_the_previous_daemons_activity(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("servers.json");
        let (view, cx) = cx.add_window_view(|_, cx| {
            Servers::with_settings(Runtime::new().unwrap(), Some(file.clone()), cx)
        });
        cx.simulate_resize(size(px(1180.), px(745.)));
        cx.run_until_parked();
        let add = cx.debug_bounds("add-server").unwrap();
        cx.simulate_click(add.center(), Default::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("paste-token").is_some());
        cx.update(|_,app|view.update(app,|s,cx|{
            s.chat=Some(Uuid::from_u128(1));
            s.run=Some(1);
            s.events.push_back(serde_json::from_value(serde_json::json!({"run_id":1,"sequence":5,"event":{"kind":"run_finished","data":"first daemon"}})).unwrap());
            let second=Profile::remote("Home Pi","https://pi.example:8700").unwrap();
            s.remember(second.clone()).unwrap();
            s.connect(second,cx);
            assert!(s.events.is_empty() && s.chats.is_empty() && s.run.is_none() && s.chat.is_none());
        }));
        cx.run_until_parked();
        cx.update(|_, app| {
            let s = view.read(app);
            assert_eq!(s.active.name, "Home Pi");
            // The GPUI test platform has no saved credentials.
            assert!(matches!(s.status, Status::Attention(_)));
        });
        assert_eq!(
            Profiles::load(&file).unwrap().selected.as_deref(),
            Some("https://pi.example:8700/")
        );
    }
}
