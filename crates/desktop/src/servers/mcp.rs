//! MCP discovery and per-tool access on the selected daemon.
use super::*;
use crate::connection::mcp::{Binding, Definition, Grant, Inventory, Launch, RemoteTool};
use std::future::Future;

#[derive(Default)]
pub(super) struct McpView {
    inventory: Option<Inventory>,
    pub(super) editor: Option<Editor>,
    expanded: Option<String>,
    request: Option<Task<()>>,
    error: Option<String>,
}

pub(super) struct Editor {
    expected: Option<Definition>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    args: Vec<Entity<TextInput>>,
    env: Entity<TextInput>,
    bindings: Vec<ToolAccess>,
    discovered: Vec<RemoteTool>,
    confirm_remove: bool,
}
struct ToolAccess {
    tool: String,
    skill: Entity<TextInput>,
    alias: Entity<TextInput>,
    grant: Grant,
}
fn input(placeholder: &'static str, value: String, cx: &mut Context<Servers>) -> Entity<TextInput> {
    cx.new(|cx| TextInput::prefilled(placeholder, value, cx))
}
fn field(
    colors: crate::theme::Palette,
    id: String,
    title: &str,
    editor: &Entity<TextInput>,
) -> impl IntoElement {
    column()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(label(title.to_owned(), 12., colors.muted))
        .child(
            div()
                .id(gpui::SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(colors.border))
                .bg(rgb(colors.surface))
                .child(editor.clone()),
        )
}
impl ToolAccess {
    fn new(binding: Binding, cx: &mut Context<Servers>) -> Self {
        Self {
            tool: binding.tool,
            skill: input("Skill", binding.skill, cx),
            alias: input("Tool name", binding.name, cx),
            grant: binding.grant,
        }
    }
    fn binding(&self, cx: &Context<Servers>) -> Binding {
        Binding {
            tool: self.tool.clone(),
            skill: self.skill.read(cx).text().trim().into(),
            name: self.alias.read(cx).text().trim().into(),
            grant: self.grant,
        }
    }
}
impl Editor {
    fn new(
        name: String,
        expected: Option<Definition>,
        discovered: Vec<RemoteTool>,
        cx: &mut Context<Servers>,
    ) -> Self {
        let definition = expected.clone().unwrap_or_default();
        Self {
            name: input("Server name", name, cx),
            command: input("Executable on the daemon", definition.launch.command, cx),
            args: definition
                .launch
                .args
                .into_iter()
                .map(|v| input("Argument", v, cx))
                .collect(),
            env: input(
                "Variable names separated by spaces",
                definition.launch.inherit_env.join(" "),
                cx,
            ),
            bindings: definition
                .bindings
                .into_iter()
                .map(|b| ToolAccess::new(b, cx))
                .collect(),
            discovered,
            expected,
            confirm_remove: false,
        }
    }
    fn launch(&self, cx: &Context<Servers>) -> Launch {
        Launch {
            command: self.command.read(cx).text().trim().into(),
            args: self
                .args
                .iter()
                .map(|a| a.read(cx).text().to_owned())
                .collect(),
            inherit_env: self
                .env
                .read(cx)
                .text()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        }
    }
}
enum Reply {
    Inventory(Inventory),
    Discovered(Vec<RemoteTool>),
    Saved,
}

impl Servers {
    pub(super) fn show_mcp(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        self.mcp = Some(McpView::default());
        self.reload_mcp(cx);
        cx.notify();
    }
    fn mcp_request(
        &mut self,
        future: impl Future<Output = Result<Reply>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.mcp.as_mut() else {
            return;
        };
        if view.request.is_some() {
            return;
        }
        view.error = None;
        let task = self.runtime.spawn(future);
        view.request = Some(cx.spawn(async move |view, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = view.update(cx, |s, cx| {
                let Some(m) = s.mcp.as_mut() else { return };
                m.request = None;
                match result {
                    Ok(Reply::Inventory(i)) => m.inventory = Some(i),
                    Ok(Reply::Discovered(tools)) => {
                        if let Some(e) = &mut m.editor {
                            e.discovered = tools;
                        }
                    }
                    Ok(Reply::Saved) => {
                        m.editor = None;
                        s.reload_mcp(cx);
                    }
                    Err(e) => m.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    pub(super) fn reload_mcp(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = self.client.clone() {
            self.mcp_request(
                async move { Ok(Reply::Inventory(client.get("mcp").await?)) },
                cx,
            );
        }
    }
    fn edit_mcp(&mut self, name: Option<&str>, cx: &mut Context<Self>) {
        let Some(m) = &mut self.mcp else { return };
        let Some(inventory) = m.inventory.as_ref().filter(|i| i.editable) else {
            return;
        };
        let server = inventory
            .servers
            .iter()
            .find(|s| Some(s.name.as_str()) == name);
        m.editor = Some(Editor::new(
            name.unwrap_or_default().into(),
            server.and_then(|s| s.configuration.clone()),
            server.map(|s| s.tools.clone()).unwrap_or_default(),
            cx,
        ));
        m.error = None;
        m.expanded = None;
        cx.notify();
    }
    fn editor(&mut self) -> Option<&mut Editor> {
        self.mcp.as_mut()?.editor.as_mut()
    }
    fn discover_mcp(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(e) = self.editor() else { return };
        let launch = e.launch(cx);
        self.mcp_request(
            async move { Ok(Reply::Discovered(client.discover_tools(&launch).await?)) },
            cx,
        );
    }
    fn save_mcp(&mut self, remove: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(e) = self.editor() else { return };
        if remove && !e.confirm_remove {
            e.confirm_remove = true;
            cx.notify();
            return;
        }
        let name = e.name.read(cx).text().trim().to_owned();
        if name.is_empty() {
            if let Some(m) = &mut self.mcp {
                m.error = Some("Enter a server name.".into());
            }
            cx.notify();
            return;
        }
        let expected = e.expected.clone();
        let replacement = (!remove).then(|| Definition {
            launch: e.launch(cx),
            bindings: e.bindings.iter().map(|b| b.binding(cx)).collect(),
        });
        self.mcp_request(
            async move {
                client
                    .configure_server(&name, expected, replacement)
                    .await?;
                Ok(Reply::Saved)
            },
            cx,
        );
    }
    fn tool_row(&self, key: String, t: &RemoteTool, edit: bool, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut line = row()
            .gap_2()
            .child(Icon::Plug.draw(15., colors.accent))
            .child(heading(colors, t.name.clone(), 14.).flex_1())
            .child(
                action_for(colors, format!("schema-{key}"), "Schema").on_click(cx.listener({
                    let key = key.clone();
                    move |s, _, _, cx| {
                        if let Some(m) = &mut s.mcp {
                            m.expanded = if m.expanded.as_ref() == Some(&key) {
                                None
                            } else {
                                Some(key.clone())
                            };
                        }
                        cx.notify();
                    }
                })),
            );
        if edit {
            let tool = t.name.clone();
            line = line.child(
                action_for(colors, format!("expose-{tool}"), "Add to skill").on_click(cx.listener(
                    move |s, _, _, cx| {
                        let Some(e) = s.editor() else { return };
                        let binding = Binding {
                            tool: tool.clone(),
                            name: tool
                                .chars()
                                .map(|c| {
                                    if c.is_ascii_alphanumeric() || c == '-' {
                                        c
                                    } else {
                                        '_'
                                    }
                                })
                                .collect(),
                            skill: e.name.read(cx).text().to_owned(),
                            grant: Grant::RequireApproval,
                        };
                        e.bindings.push(ToolAccess::new(binding, cx));
                        cx.notify();
                    },
                )),
            );
        }
        let mut item = column()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(rgb(colors.border))
            .child(line);
        if let Some(description) = &t.description {
            item = item.child(label(description.clone(), 12., colors.muted));
        }
        if self.mcp.as_ref().and_then(|m| m.expanded.as_ref()) == Some(&key) {
            item = item.child(
                label(
                    serde_json::to_string_pretty(&t.parameters).unwrap_or_default(),
                    12.,
                    colors.text,
                )
                .bg(rgb(colors.sidebar))
                .p_2(),
            );
        }
        item
    }
    fn mcp_editor(&self, e: &Editor, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut form = column().gap_3().w_full().max_w(px(760.));
        form = if e.expected.is_some() {
            form.child(heading(colors, e.name.read(cx).text().to_owned(), 17.))
        } else {
            form.child(field(colors, "mcp-name".into(), "Server name", &e.name))
        };
        form = form.child(field(
            colors,
            "mcp-command".into(),
            "Executable on daemon",
            &e.command,
        ));
        for (i, arg) in e.args.iter().enumerate() {
            form = form.child(
                row()
                    .gap_2()
                    .child(field(
                        colors,
                        format!("mcp-arg-{i}"),
                        &format!("Argument {}", i + 1),
                        arg,
                    ))
                    .child(
                        action_for(colors, format!("remove-arg-{i}"), "Remove").on_click(
                            cx.listener(move |s, _, _, cx| {
                                if let Some(e) = s.editor() {
                                    e.args.remove(i);
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            );
        }
        form = form
            .child(
                row().child(action(colors, "add-arg", "Add argument", false).on_click(
                    cx.listener(|s, _, _, cx| {
                        if let Some(e) = s.editor() {
                            e.args.push(input("Argument", String::new(), cx));
                        }
                        cx.notify();
                    }),
                )),
            )
            .child(field(
                colors,
                "mcp-env".into(),
                "Inherit environment variables",
                &e.env,
            ))
            .child(
                row().child(
                    action(colors, "discover-mcp", "Discover tools", false)
                        .on_click(cx.listener(|s, _, _, cx| s.discover_mcp(cx))),
                ),
            );
        for tool in &e.discovered {
            form = form.child(self.tool_row(tool.name.clone(), tool, true, cx));
        }
        if !e.bindings.is_empty() {
            form = form.child(heading(colors, "Skill access", 15.));
        }
        for (i, b) in e.bindings.iter().enumerate() {
            let mut grants = row().gap_2();
            for (grant, key) in [
                (Grant::RequireApproval, "approval"),
                (Grant::Allow, "allow"),
            ] {
                let selected = b.grant == grant;
                grants = grants.child(
                    hit(format!("grant-{key}-{i}"))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(if selected {
                            colors.accent
                        } else {
                            colors.surface
                        }))
                        .child(label(
                            grant.label(),
                            12.,
                            if selected {
                                colors.on_accent
                            } else {
                                colors.text
                            },
                        ))
                        .on_click(cx.listener(move |s, _, _, cx| {
                            if let Some(e) = s.editor() {
                                e.bindings[i].grant = grant;
                            }
                            cx.notify();
                        })),
                );
            }
            form = form.child(
                column()
                    .gap_2()
                    .bg(rgb(colors.sidebar))
                    .p_3()
                    .rounded_lg()
                    .child(heading(colors, b.tool.clone(), 13.))
                    .child(
                        row()
                            .gap_2()
                            .child(field(
                                colors,
                                format!("skill-{i}"),
                                "Wakefold skill",
                                &b.skill,
                            ))
                            .child(field(
                                colors,
                                format!("alias-{i}"),
                                "Tool name in skill",
                                &b.alias,
                            )),
                    )
                    .child(grants.child(
                        action_for(colors, format!("unmap-{i}"), "Remove from skill").on_click(
                            cx.listener(move |s, _, _, cx| {
                                if let Some(e) = s.editor() {
                                    e.bindings.remove(i);
                                }
                                cx.notify();
                            }),
                        ),
                    )),
            );
        }
        let mut buttons = row()
            .gap_2()
            .child(
                action(colors, "save-mcp", "Save configuration", true)
                    .on_click(cx.listener(|s, _, _, cx| s.save_mcp(false, cx))),
            )
            .child(
                action(colors, "cancel-mcp", "Cancel", false).on_click(cx.listener(
                    |s, _, _, cx| {
                        if let Some(m) = &mut s.mcp {
                            m.editor = None;
                            m.error = None;
                        }
                        s.reload_mcp(cx);
                        cx.notify();
                    },
                )),
            );
        if e.expected.is_some() {
            buttons = buttons.child(
                action(
                    colors,
                    "remove-mcp",
                    if e.confirm_remove {
                        "Confirm removal"
                    } else {
                        "Remove server"
                    },
                    false,
                )
                .on_click(cx.listener(|s, _, _, cx| s.save_mcp(true, cx))),
            );
        }
        form.child(label(
            "Saved changes take effect when this daemon restarts.",
            12.,
            colors.muted,
        ))
        .child(buttons)
    }
    pub(super) fn mcp_view(&self, m: &McpView, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut content = column()
            .id("mcp-content")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_3();
        if let Status::Reconnecting(reason) | Status::Attention(reason) = &self.status {
            content = content.child(label(reason.clone(), 13., colors.warning));
        }
        if let Some(error) = &m.error {
            content = content.child(label(error.clone(), 13., colors.warning));
        }
        if m.request.is_some() {
            content = content.child(label("Contacting daemon…", 14., colors.muted));
        } else if let Some(e) = &m.editor {
            content = content.child(self.mcp_editor(e, cx));
        } else {
            let mut toolbar = row().gap_2().child(
                action(colors, "reload-mcp", "Refresh", false)
                    .on_click(cx.listener(|s, _, _, cx| s.reload_mcp(cx))),
            );
            if let Some(i) = &m.inventory {
                if i.editable {
                    toolbar = toolbar.child(
                        action(colors, "new-mcp", "Add MCP server", true)
                            .on_click(cx.listener(|s, _, _, cx| s.edit_mcp(None, cx))),
                    );
                } else {
                    let instructions = "To edit servers, enable allow_mcp_configuration in the daemon configuration.";
                    content = content.child(label(instructions, 13., colors.muted));
                }
                if i.servers.is_empty() {
                    content = content.child(label("No MCP servers configured.", 14., colors.muted));
                }
                content = content.child(toolbar);
                for server in &i.servers {
                    let mut top = row()
                        .gap_2()
                        .child(heading(colors, server.name.clone(), 16.).flex_1());
                    if i.editable && server.configuration.is_some() {
                        let name = server.name.clone();
                        top =
                            top.child(action_for(colors, format!("edit-{name}"), "Edit").on_click(
                                cx.listener(move |s, _, _, cx| s.edit_mcp(Some(&name), cx)),
                            ));
                    }
                    let mut card = column()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(colors.border))
                        .p_3()
                        .gap_2()
                        .max_w(px(760.))
                        .child(top);
                    if server.restart_required {
                        card = card.child(label("Pending daemon restart", 13., colors.warning));
                    }
                    for t in &server.tools {
                        card = card.child(self.tool_row(
                            format!("{}-{}", server.name, t.name),
                            t,
                            false,
                            cx,
                        ));
                    }
                    for b in &server.bindings {
                        card = card.child(label(
                            format!("{} → {}.{} · {}", b.tool, b.skill, b.name, b.grant.label()),
                            13.,
                            colors.text,
                        ));
                    }
                    content = content.child(card);
                }
            } else {
                content = content.child(toolbar);
            }
        }
        column()
            .flex_1()
            .min_h_0()
            .child(heading(colors, "MCP servers", 19.).px_4().py_3())
            .child(content)
    }
}
fn action_for(colors: crate::theme::Palette, id: String, name: &str) -> gpui::Stateful<Div> {
    hit(id)
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(colors.tint))
        .child(label(name.to_owned(), 12., colors.accent))
}
