use super::{Desktop, TextInput};
use gpui::{Context, Entity, ScrollHandle, prelude::*};

pub struct Worker {
    pub name: &'static str,
    pub task: &'static str,
    pub finding: &'static str,
}
pub struct Artifact {
    pub filename: &'static str,
    pub title: &'static str,
    pub sections: &'static [(&'static str, &'static str)],
}
impl Artifact {
    pub fn markdown(&self) -> String {
        let mut s = format!("# {}\n", self.title);
        for (heading, body) in self.sections {
            s.push_str(&format!("\n## {heading}\n\n{body}\n"));
        }
        s
    }
}
const BRIEF: Artifact = Artifact {
    filename: "field-station-brief.md",
    title: "Field station brief",
    sections: &[
        (
            "Store readings locally",
            "Write each reading to SQLite before attempting an upload. Keep unacknowledged readings on disk across power loss.",
        ),
        (
            "Resume an upload",
            "Give each batch a stable ID. Retry that ID after a disconnect and remove local readings only after the receiver acknowledges them.",
        ),
    ],
};
const CHECKLIST: Artifact = Artifact {
    filename: "field-trial-checklist.md",
    title: "Field trial checklist",
    sections: &[
        (
            "Power cycle",
            "Collect ten readings without a network connection. Restart the logger and confirm all ten are still queued.",
        ),
        (
            "Reconnect",
            "Interrupt the first upload before the acknowledgement. Reconnect and verify the receiver records each reading once.",
        ),
    ],
};
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Publication {
    Pending,
    Published,
    Declined,
}
pub struct Turn {
    pub prompt: String,
    pub reply: Option<&'static str>,
    pub workers: &'static [Worker],
    pub expanded: bool,
    pub artifact: Option<(&'static Artifact, Publication)>,
}
impl Turn {
    pub fn message(prompt: String) -> Self {
        Self {
            prompt,
            reply: None,
            workers: &[],
            expanded: false,
            artifact: None,
        }
    }
    pub fn published(&self) -> Option<&'static Artifact> {
        self.artifact
            .and_then(|(file, state)| (state == Publication::Published).then_some(file))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Workspace,
    Worker(usize, usize),
    Artifact(usize),
}
pub struct Chat {
    pub title: String,
    pub turns: Vec<Turn>,
    pub draft: Entity<TextInput>,
    pub scroll: ScrollHandle,
    pub tabs: Vec<Tab>,
    pub active: Option<Tab>,
}
impl Chat {
    pub fn new(title: &str, cx: &mut Context<Desktop>) -> Self {
        Self {
            title: title.into(),
            turns: vec![],
            draft: cx.new(TextInput::new),
            scroll: ScrollHandle::new(),
            tabs: vec![],
            active: None,
        }
    }
    pub fn waiting(&self) -> bool {
        self.turns
            .iter()
            .any(|t| matches!(t.artifact, Some((_, Publication::Pending))))
    }
    pub fn open(&mut self, tab: Tab) {
        if !self.tabs.contains(&tab) {
            self.tabs.push(tab);
        }
        self.active = Some(tab);
    }
    pub fn close(&mut self, tab: Tab) {
        if let Some(i) = self.tabs.iter().position(|&t| t == tab) {
            self.tabs.remove(i);
            if self.active == Some(tab) {
                self.active = self.tabs.get(i).or(self.tabs.last()).copied();
            }
        }
    }
    pub fn tab_title(&self, tab: Tab) -> &str {
        match tab {
            Tab::Workspace => "Workspace",
            Tab::Worker(t, w) => self.turns[t].workers[w].name,
            Tab::Artifact(t) => {
                self.turns[t]
                    .published()
                    .expect("published artifact tab")
                    .filename
            }
        }
    }
}
pub fn examples(cx: &mut Context<Desktop>) -> Vec<Chat> {
    let mut report = Chat::new("Field station brief", cx);
    report.turns.push(Turn{
        prompt:"Plan an offline logging workflow for our field station. How do we keep readings safe when the connection drops?".into(),
        reply:Some("Keep a local queue on the logger. Write each reading to disk first, then upload batches with a stable ID so an interrupted transfer can resume safely."),
        workers:&[Worker{name:"Local storage",task:"Find a way to keep sensor readings safe through a power cut.",finding:"Store each reading in a SQLite transaction. A committed reading stays in the queue after a restart, even if the network was down."},
                  Worker{name:"Upload recovery",task:"Work out what the logger should do if the connection drops halfway through an upload.",finding:"Keep the batch ID across retries. The receiver can acknowledge a repeated batch without inserting its readings a second time."}],
        expanded:true,artifact:Some((&BRIEF,Publication::Pending)),
    });
    let mut notes = Chat::new("Deployment notes", cx);
    notes.turns.push(Turn{prompt:"What should we check before the field trial?".into(),reply:Some("Start with a power-cycle test, then interrupt an upload. The checklist gives you a pass condition for each test."),workers:&[],expanded:false,artifact:Some((&CHECKLIST,Publication::Published))});
    vec![notes, report]
}
