//! Immutable publications in the selected chat.
use super::*;
use crate::connection::ArtifactRevision;
use std::path::Path;

#[derive(Default)]
pub(super) struct Artifacts {
    pub(super) revisions: Vec<ArtifactRevision>,
    pub(super) visible: bool,
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
        let mut controls = row().gap_2();
        let id = file.artifact_id;
        let revision = file.revision;
        for (open, name) in [(true, "Open"), (false, "Download")] {
            if open && document_extension(&file.filename).is_none() {
                continue;
            }
            let file = file.clone();
            controls = controls.child(
                hit(format!("{name}-{id}-{revision}"))
                    .px_2()
                    .py_1()
                    .bg(rgb(WHITE))
                    .child(label(name, 12., GREEN))
                    .on_click(
                        cx.listener(move |s, _, _, cx| s.transfer_artifact(file.clone(), open, cx)),
                    ),
            );
        }
        let mut card = column().gap_1().p_3().rounded_lg().bg(rgb(RAIL)).child(
            row()
                .gap_2()
                .flex_wrap()
                .child(Icon::File.draw(18., GREEN))
                .child(
                    column()
                        .flex_1()
                        .min_w_0()
                        .child(heading(file.title.clone(), 14.))
                        .child(label(
                            format!(
                                "{} · Revision {} · {} bytes",
                                file.filename, revision, file.size_bytes
                            ),
                            12.,
                            MUTED,
                        )),
                )
                .child(controls),
        );
        if let Some((a, r, msg)) = &self.artifacts.transfer
            && (*a, *r) == (id, revision)
        {
            card = card.child(label(msg.clone(), 12., MUTED));
        }
        card
    }

    pub(super) fn artifact_library(&self, cx: &mut Context<Self>) -> Div {
        let mut list = column()
            .id("artifact-library")
            .overflow_y_scroll()
            .flex_1()
            .gap_2()
            .p_4();
        if self.artifacts.revisions.is_empty() {
            list = list.child(label("This chat has no published files yet.", 14., MUTED));
        }
        for revision in &self.artifacts.revisions {
            list = list.child(self.artifact_card(revision, cx));
        }
        column()
            .flex_1()
            .min_h_0()
            .child(
                row()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .child(heading("Chat artifacts", 18.))
                    .child(
                        action("back-to-chat", "Back to chat", false).on_click(cx.listener(
                            |s, _, _, cx| {
                                s.artifacts.visible = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(list)
    }
}
