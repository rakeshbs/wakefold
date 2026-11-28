//! Run indices and GPUI's measured heights share the same root-turn order.
use super::Run;
use gpui::{ListAlignment, ListOffset, ListState, Pixels, px};
use std::{cell::Cell, collections::BTreeMap};

pub(super) struct Turn {
    pub root: usize,
    pub children: Vec<usize>,
}
pub(super) struct Transcript {
    pub list: ListState,
    pub turns: Vec<Turn>,
    follow: Cell<bool>,
}
impl Default for Transcript {
    fn default() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(200.)).measure_all(),
            turns: Vec::new(),
            follow: Cell::new(true),
        }
    }
}
impl Transcript {
    pub fn follow_latest(&self) {
        self.follow.set(true);
    }
    pub fn follow_if_near_bottom(&self) {
        let remaining =
            self.list.max_offset_for_scrollbar().y + self.list.scroll_px_offset_for_scrollbar().y;
        if remaining < px(100.) {
            self.follow_latest();
        }
    }
    pub fn apply_follow(&self) {
        if self.follow.replace(false) {
            self.list.scroll_to(ListOffset {
                item_ix: self.turns.len(),
                offset_in_item: Pixels::ZERO,
            });
        }
    }

    pub fn invalidate_hidden_turn(&self, runs: &[Run], root_id: i64) {
        let Some(index) = self
            .turns
            .iter()
            .position(|turn| runs[turn.root].run_id == root_id)
        else {
            return;
        };
        // Visible rows are measured on every layout. An off-screen event can also
        // grow a reply or its activity log while the reader is elsewhere.
        if self
            .list
            .bounds_for_item(index)
            .is_some_and(|row| row.intersects(&self.list.viewport_bounds()))
        {
            return;
        }
        self.list.splice(index..index + 1, 1);
        self.list.clone().measure_all();
    }

    pub fn sync(&mut self, previous: &[Run], runs: &[Run], cards_changed: bool) {
        let mut children = BTreeMap::<i64, Vec<usize>>::new();
        for (index, run) in runs.iter().enumerate() {
            if let Some(parent) = run.spawned_by_run_id {
                children.entry(parent).or_default().push(index);
            }
        }
        let turns: Vec<_> = runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.parent_agent_id.is_none())
            .map(|(root, r)| Turn {
                root,
                children: children.remove(&r.run_id).unwrap_or_default(),
            })
            .collect();

        let anchor = self.list.logical_scroll_top();
        let anchor_id = self
            .turns
            .get(anchor.item_ix)
            .map(|row| previous[row.root].run_id);
        let shared = self
            .turns
            .iter()
            .zip(&turns)
            .take_while(|(old, new)| previous[old.root].run_id == runs[new.root].run_id)
            .count();
        for (index, (old, new)) in self.turns.iter().zip(&turns).take(shared).enumerate() {
            if (index + 1 == self.turns.len()) != (index + 1 == turns.len())
                || cards_changed
                || previous[old.root] != runs[new.root]
                || !old
                    .children
                    .iter()
                    .map(|i| &previous[*i])
                    .eq(new.children.iter().map(|i| &runs[*i]))
            {
                self.list.splice(index..index + 1, 1);
            }
        }
        self.list
            .splice(shared..self.turns.len(), turns.len() - shared);
        if let Some(index) =
            anchor_id.and_then(|id| turns.iter().position(|row| runs[row.root].run_id == id))
        {
            self.list.scroll_to(ListOffset {
                item_ix: index,
                ..anchor
            });
        }
        // Compute accurate thumb travel on data changes. Existing heights are reused.
        self.list = self.list.clone().measure_all();
        self.turns = turns;
    }
}
