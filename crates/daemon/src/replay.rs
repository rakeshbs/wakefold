//! Inspection of saved run journals in causal order.

use std::collections::HashSet;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;

use serde::Serialize;
use wakefold::{JournalReplay, RunEvent, WaitOn};

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
    let mut runs = vec![(agent_id, 0)];
    let mut seen = HashSet::from([agent_id]);
    while let Some((agent_id, after)) = runs.pop() {
        let page = journal
            .run_event_page(agent_id, after, PAGE_SIZE)?
            .ok_or_else(|| format!("unknown run {agent_id}"))?;
        let mut next_page = page
            .entries
            .last()
            .filter(|_| page.entries.len() == PAGE_SIZE.get())
            .map(|entry| entry.sequence);
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
            if let RunEvent::WaitStarted(wait) = &entry.event
                && matches!(wait.on, WaitOn::Delegation { .. })
            {
                let child = journal
                    .worker_id(agent_id, &wait.id)?
                    .filter(|&child| seen.insert(child))
                    .ok_or_else(|| format!("invalid worker link for delegation {}", wait.id))?;
                // The parent's result depends on this worker. Read the
                // worker before continuing at the parent's next event.
                runs.push((agent_id, entry.sequence));
                runs.push((child, 0));
                next_page = None;
                break;
            }
        }
        if let Some(after) = next_page {
            runs.push((agent_id, after));
        }
    }
    Ok(())
}
