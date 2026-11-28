//! Immutable publications in the selected chat.
use super::*;
use crate::connection::ArtifactRevision;
use std::path::Path;

#[derive(Default)]
pub(super) struct Artifacts {
    pub(super) revisions: Vec<ArtifactRevision>,
    transfer: Option<(i64, u64, String)>,
}

fn document_extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "txt"
            | "md"
            | "csv"
            | "tsv"
            | "pdf"
            | "xlsx"
            | "docx"
            | "pptx"
            | "ods"
            | "odt"
            | "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "json"
    )
    .then_some(ext)
}

impl Servers {
    fn transfer_artifact(&mut self, file: ArtifactRevision, open: bool, cx: &mut Context<Self>) {
        let (Some(client), Some(chat)) = (self.client.clone(), self.chat) else {
            return;
        };
        let url = self.active.url.clone();
        let directory = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
        let suggested = file.filename.replace(['/', '\\', ':'], "_");
        let choose = (!open).then(|| cx.prompt_for_new_path(&directory, Some(&suggested)));
        let id = file.artifact_id;
        let revision = file.revision;
        let runtime = self.runtime.handle().clone();
        self.artifacts.transfer = Some((id, revision, "Downloading…".into()));
        cx.spawn(async move |view, cx| {
            let transfer = async {
                let (directory, path) = if let Some(dialog) = choose {
                    let Some(path) = dialog.await?? else {
                        return Ok(None);
                    };
                    (None, path)
                } else {
                    let extension = document_extension(&file.filename)
                        .context("Choose Download for this file type")?;
                    let directory = tempfile::tempdir()?;
                    let path = directory
                        .path()
                        .join(format!("artifact-{id}-r{revision}.{extension}"));
                    (Some(directory), path)
                };
                runtime
                    .spawn(async move {
                        client.download_revision(chat, &file, &path).await?;
                        Ok::<_, anyhow::Error>(Some((directory, path)))
                    })
                    .await?
            }
            .await;
            let _ = view.update(cx, |s, cx| {
                if s.active.url != url || s.chat != Some(chat) {
                    return;
                }
                s.artifacts.transfer = match transfer {
                    Ok(Some((temp, path))) => {
                        if let Some(temp) = temp {
                            s.opened_files.push(temp);
                            cx.open_with_system(&path);
                        }
                        Some((
                            id,
                            revision,
                            if open {
                                "Opened".into()
                            } else {
                                format!("Saved to {}", path.display())
                            },
                        ))
                    }
                    Ok(None) => None,
                    Err(e) => Some((id, revision, format!("{e:#}"))),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn artifact_card(&self, file: &ArtifactRevision, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let panel = Panel::File(file.artifact_id, file.revision);
        let card = row()
            .min_w_0()
            .flex_none()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(colors.border))
            .bg(rgb(colors.surface))
            .child(
                hit(format!("view-{}-{}", file.artifact_id, file.revision))
                    .min_w_0()
                    .flex_1()
                    .gap_2()
                    .child(tile(Icon::File, colors.tint, colors.accent))
                    .child(
                        column()
                            .min_w_0()
                            .flex_1()
                            .child(heading(colors, file.title.clone(), 14.).truncate())
                            .child(
                                label(
                                    format!("{} · Revision {}", file.filename, file.revision),
                                    12.,
                                    colors.muted,
                                )
                                .truncate(),
                            ),
                    )
                    .child(Icon::Right.draw(14., colors.accent))
                    .on_click(cx.listener(move |s, _, _, cx| s.open_panel(panel, cx))),
            )
            .child(self.download_button(file, cx));
        let mut body = column().flex_none().child(card);
        if let Some((id, rev, message)) = &self.artifacts.transfer
            && (*id, *rev) == (file.artifact_id, file.revision)
        {
            body = body.child(label(message.clone(), 12., colors.muted));
        }
        body
    }
    fn download_button(
        &self,
        file: &ArtifactRevision,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let colors = crate::theme::palette(cx);
        let file = file.clone();
        hit(format!("Download-{}-{}", file.artifact_id, file.revision))
            .p_1()
            .child(label("Download", 12., colors.accent))
            .on_click(cx.listener(move |s, _, _, cx| s.transfer_artifact(file.clone(), false, cx)))
    }
    pub(super) fn artifact_library(&self, cx: &mut Context<Self>) -> Div {
        let colors = crate::theme::palette(cx);
        let mut list = column().gap_2().p_4();
        if self.artifacts.revisions.is_empty() {
            list = list.child(label(
                "This chat has no published files yet.",
                14.,
                colors.muted,
            ));
        }
        for file in &self.artifacts.revisions {
            list = list.child(self.artifact_card(file, cx));
        }
        list
    }
    pub(super) fn load_preview(&mut self, file: ArtifactRevision, cx: &mut Context<Self>) {
        let (Some(client), Some(chat)) = (self.client.clone(), self.chat) else {
            return;
        };
        let url = self.active.url.clone();
        let panel = Panel::File(file.artifact_id, file.revision);
        let task = self
            .runtime
            .spawn(async move { preview_revision(&client, chat, &file).await });
        cx.spawn(async move |view, cx| {
            let preview = match task.await {
                Ok(Ok(preview)) => preview,
                Ok(Err(e)) => Preview::Notice(format!("Could not preview this revision: {e:#}")),
                Err(e) => Preview::Notice(format!("Preview interrupted: {e}")),
            };
            let _ = view.update(cx, |s, cx| {
                if s.active.url == url && s.chat == Some(chat) {
                    s.dock.preview_loaded(panel, preview);
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn file_preview(
        &self,
        id: i64,
        rev: u64,
        preview: Option<&Preview>,
        cx: &mut Context<Self>,
    ) -> Div {
        let colors = crate::theme::palette(cx);
        let Some(file) = self
            .artifacts
            .revisions
            .iter()
            .find(|f| (f.artifact_id, f.revision) == (id, rev))
        else {
            return label("Loading revision…", 14., colors.muted);
        };
        let mut actions = row()
            .flex_wrap()
            .gap_2()
            .child(self.download_button(file, cx));
        if document_extension(&file.filename).is_some() {
            let file = file.clone();
            actions = actions.child(
                hit("open-externally")
                    .p_1()
                    .child(label("Open in app", 12., colors.accent))
                    .on_click(
                        cx.listener(move |s, _, _, cx| s.transfer_artifact(file.clone(), true, cx)),
                    ),
            );
        }
        let mut body = column()
            .min_w_0()
            .p_4()
            .gap_3()
            .child(heading(colors, file.filename.clone(), 14.))
            .child(actions);
        if let Some((a, r, msg)) = &self.artifacts.transfer
            && (*a, *r) == (id, rev)
        {
            body = body.child(label(msg.clone(), 12., colors.muted));
        }
        match preview {
            None => body.child(label("Loading preview…", 14., colors.muted)),
            Some(Preview::Notice(message)) => body.child(label(message.clone(), 14., colors.muted)),
            Some(Preview::Text {
                text,
                markdown: true,
            }) => body.child(crate::markdown::markdown(colors, text)),
            Some(Preview::Text { text, .. }) => body.child(
                label(text.clone(), 13., colors.text)
                    .font_family("monospace")
                    .line_height(px(20.)),
            ),
            Some(Preview::Image(image, caption)) => {
                if let Some(caption) = caption {
                    body = body.child(label(*caption, 12., colors.muted));
                }
                body.child(
                    gpui::img(image.clone())
                        .w_full()
                        .h(px(420.))
                        .object_fit(gpui::ObjectFit::Contain),
                )
            }
        }
    }
}

pub(super) enum Preview {
    Text { text: String, markdown: bool },
    Image(std::sync::Arc<gpui::Image>, Option<&'static str>),
    Notice(String),
}
async fn preview_revision(client: &Daemon, chat: Uuid, file: &ArtifactRevision) -> Result<Preview> {
    use gpui::ImageFormat;
    let ext = Path::new(&file.filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let image = match ext.as_str() {
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::Webp),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    };
    let text = matches!(
        ext.as_str(),
        "txt"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "log"
            | "html"
            | "xml"
            | "svg"
            | "rs"
            | "py"
            | "js"
            | "css"
    );
    let document = cfg!(target_os = "macos")
        && matches!(
            ext.as_str(),
            "pdf" | "docx" | "pptx" | "xlsx" | "odt" | "ods"
        );
    if !(text || image.is_some() || document) {
        return Ok(Preview::Notice("Download this file to view it.".into()));
    }
    let limit = if text { 256 * 1024 } else { 16 * 1024 * 1024 };
    if file.size_bytes > limit {
        return Ok(Preview::Notice(
            "This file is too large for an inline preview. Download it to see the full revision."
                .into(),
        ));
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(format!("preview.{ext}"));
    client.download_revision(chat, file, &path).await?;
    #[cfg(target_os = "macos")]
    if document {
        let mut command = tokio::process::Command::new("/usr/bin/qlmanage");
        command
            .args(["-t", "-s", "1400", "-o"])
            .arg(directory.path())
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if tokio::time::timeout(std::time::Duration::from_secs(12), command.status())
            .await
            .is_ok_and(|r| r.is_ok_and(|s| s.success()))
            && let Ok(bytes) =
                tokio::fs::read(directory.path().join(format!("preview.{ext}.png"))).await
        {
            return Ok(Preview::Image(
                gpui::Image::from_bytes(ImageFormat::Png, bytes).into(),
                Some("Quick Look preview"),
            ));
        }
        return Ok(Preview::Notice("Open in app to view this document.".into()));
    }
    let bytes = tokio::fs::read(path).await?;
    if let Some(format) = image {
        Ok(Preview::Image(
            gpui::Image::from_bytes(format, bytes).into(),
            None,
        ))
    } else {
        let text = String::from_utf8(bytes).context("File is not UTF-8 text")?;
        Ok(Preview::Text {
            text,
            markdown: matches!(ext.as_str(), "md" | "markdown"),
        })
    }
}
