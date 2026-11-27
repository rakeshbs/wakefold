//! Inspection of saved run journals grouped by agent.

use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;

use serde::Serialize;
use wakefold::{JournalReplay, RunEvent};

use crate::DaemonError;

const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(128).expect("page size is nonzero");

#[derive(Serialize)]
struct ReplayEvent<'a> {
    agent_id: i64,
    sequence: u64,
    event: &'a RunEvent,
}

pub(crate) fn write(
    state: &Path,
    agent_id: i64,
    mut output: impl Write,
) -> Result<(), DaemonError> {
    let journal = JournalReplay::open(state)?;
    let mut runs = vec![agent_id];
    runs.extend(journal.child_ids(agent_id)?);
    for agent_id in runs {
        let mut after = 0;
        loop {
            let page = journal
                .run_event_page(agent_id, after, PAGE_SIZE)?
                .ok_or_else(|| format!("unknown run {agent_id}"))?;
            let count = page.entries.len();
            for entry in page.entries {
                serde_json::to_writer(
                    &mut output,
                    &ReplayEvent {
                        agent_id,
                        sequence: entry.sequence,
                        event: &entry.event,
                    },
                )?;
                output.write_all(b"\n")?;
                after = entry.sequence;
            }
            if count < PAGE_SIZE.get() {
                break;
            }
        }
    }
    Ok(())
}
