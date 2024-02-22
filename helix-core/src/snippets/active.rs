use std::ops::{Index, IndexMut};

use hashbrown::HashSet;
use helix_stdx::range::is_subset;
use helix_stdx::Range;

use crate::movement::Direction;
use crate::snippets::render::{self, Tabstop};
use crate::snippets::TabstopIdx;
use crate::{selection, Assoc, ChangeSet, Selection};

pub struct ActiveSnippet {
    ranges: Vec<Range>,
    active_tabstops: HashSet<TabstopIdx>,
    active_tabstop: TabstopIdx,
    tabstops: Vec<Tabstop>,
}

impl Index<TabstopIdx> for ActiveSnippet {
    type Output = Tabstop;
    fn index(&self, index: TabstopIdx) -> &Tabstop {
        &self.tabstops[index.0]
    }
}

impl IndexMut<TabstopIdx> for ActiveSnippet {
    fn index_mut(&mut self, index: TabstopIdx) -> &mut Tabstop {
        &mut self.tabstops[index.0]
    }
}

impl ActiveSnippet {
    pub fn new(
        primary_idx: usize,
        direction: Direction,
        snippet: render::Snippet,
    ) -> (Option<Self>, Selection) {
        let mut snippet = Self {
            ranges: snippet.ranges,
            tabstops: snippet.tabstops,
            active_tabstops: HashSet::new(),
            active_tabstop: TabstopIdx(0),
        };
        let selection = snippet
            .activate_tabstop(primary_idx, direction)
            .expect("before the first call to map() all tabstops must be valid");
        let res = (snippet.tabstops.len() != 1).then_some(snippet);
        (res, selection)
    }
    pub fn is_valid(&self, new_selection: &Selection) -> bool {
        let active_tab_stop = &self[self.active_tabstop];
        is_subset(
            active_tab_stop.ranges.iter().copied(),
            new_selection.range_bounds(),
        )
    }

    /// maps the active snippets trough a `ChangeSet` updating all tabstop ranges
    pub fn map(&mut self, changes: &ChangeSet) {
        let positions_to_map = self.ranges.iter_mut().flat_map(|range| {
            [
                (&mut range.start, Assoc::After),
                (&mut range.end, Assoc::Before),
            ]
        });
        changes.update_positions(positions_to_map);

        for (i, tabstop) in self.tabstops.iter_mut().enumerate() {
            if self.active_tabstops.contains(&TabstopIdx(i)) {
                let positions_to_map = tabstop.ranges.iter_mut().flat_map(|range| {
                    let end_assoc = if range.start == range.end {
                        Assoc::Before
                    } else {
                        Assoc::After
                    };
                    [
                        (&mut range.start, Assoc::Before),
                        (&mut range.end, end_assoc),
                    ]
                });
                changes.update_positions(positions_to_map);
            } else {
                let positions_to_map = tabstop.ranges.iter_mut().flat_map(|range| {
                    let end_assoc = if range.start == range.end {
                        Assoc::After
                    } else {
                        Assoc::Before
                    };
                    [
                        (&mut range.start, Assoc::After),
                        (&mut range.end, end_assoc),
                    ]
                });
                changes.update_positions(positions_to_map);
            }
            let mut snippet_ranges = self.ranges.iter();
            let mut snippet_range = snippet_ranges.next().unwrap();
            let mut tabstop_i = 0;
            let mut prev = Range { start: 0, end: 0 };
            let num_ranges = tabstop.ranges.len() / self.ranges.len();
            tabstop.ranges.retain_mut(|range| {
                if tabstop_i == num_ranges {
                    snippet_range = snippet_ranges.next().unwrap();
                    tabstop_i = 0;
                }
                tabstop_i += 1;
                let retain = snippet_range.start <= snippet_range.end;
                if retain {
                    range.start = range.start.max(snippet_range.start);
                    range.end = range.end.max(range.start).min(snippet_range.end);
                    // garunteed by assoc
                    debug_assert!(prev.start <= range.start);
                    debug_assert!(range.start <= range.end);
                    if prev.end > range.start {
                        // not really sure what to do in this case. It shouldn't
                        // really occur in practice% the below just ensures
                        // our invriants hold
                        range.start = prev.end;
                        range.end = range.end.max(range.start)
                    }
                    prev = *range;
                }
                retain
            });
        }
    }

    pub fn next_tabstop(&mut self, current_selection: &Selection) -> Option<(Selection, bool)> {
        let primary_idx = self.primary_idx(current_selection);
        while self.active_tabstop.0 + 1 < self.tabstops.len() {
            self.active_tabstop.0 += 1;
            let selection = self.activate_tabstop(primary_idx, Direction::Forward);
            if let Some(selection) = selection {
                return Some((selection, self.active_tabstop.0 + 1 == self.tabstops.len()));
            }
        }

        None
    }

    pub fn prev_tabstop(&mut self, current_selection: &Selection) -> Option<Selection> {
        let primary_idx = self.primary_idx(current_selection);
        while self.active_tabstop.0 != 0 {
            self.active_tabstop.0 -= 1;
            let selection = self.activate_tabstop(primary_idx, Direction::Backward);
            if let Some(selection) = selection {
                return Some(selection);
            }
        }
        None
    }
    // computes the primary idx adjust for the number of cursors in the current tabstop
    fn primary_idx(&self, current_selection: &Selection) -> usize {
        let primary: Range = current_selection.primary().into();
        self.ranges
            .iter()
            .position(|&range| range.contains(primary))
            .expect("active snippet must be valid")
    }

    fn activate_tabstop(&mut self, primary_idx: usize, direction: Direction) -> Option<Selection> {
        let tabstop = &self[self.active_tabstop];
        if tabstop.has_placeholder() && tabstop.ranges.iter().all(|range| range.is_empty()) {
            return None;
        }
        self.active_tabstops.clear();
        self.active_tabstops.insert(self.active_tabstop);
        let mut parent = self[self.active_tabstop].parent;
        while let Some(tabstop) = parent {
            self.active_tabstops.insert(tabstop);
            parent = self[tabstop].parent;
        }
        let tabstop = &self[self.active_tabstop];
        // TODO: if the user removes the seleciton(s) in one snippet (but
        // there are still other cursors in other snippets) and jumps to the
        // next tabstop the selection in that tabstop is restored (at the
        // next tabstop). This could be annoying since its not possible to
        // remove a snippet cursor until the snippet is complete. On the other
        // hand it may be useful since the user may just have meant to edit
        // a subselection (like with s) of the tabstops and so the selection
        // removal was just temporary. Potentially this could have some sort of
        // seperate keymap
        let selection = Selection::new(
            self[self.active_tabstop]
                .ranges
                .iter()
                .map(|&range| {
                    let mut range = selection::Range::new(range.start, range.end);
                    if direction == Direction::Backward {
                        range = range.flip()
                    }
                    range
                })
                .collect(),
            primary_idx * (tabstop.ranges.len() / self.ranges.len()),
        );
        Some(selection)
    }

    pub fn insert_snippet(&mut self, snippet: render::Snippet) {
        let mut cnt = 0;
        let parent = self[self.active_tabstop].parent;
        let tabstops = snippet.tabstops.into_iter().map(|mut tabstop| {
            cnt += 1;
            if let Some(parent) = &mut tabstop.parent {
                parent.0 += self.active_tabstop.0;
            } else {
                tabstop.parent = parent;
            }
            tabstop
        });
        self.tabstops
            .splice(self.active_tabstop.0..=self.active_tabstop.0, tabstops);
    }
}
