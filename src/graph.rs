//! Lane-based commit graph layout.
//!
//! For each commit, we record which "lane" (column) it occupies and which
//! lanes are active above and below it. The renderer turns that into ASCII
//! using box-drawing characters with proper corners and crossings.

use gix::ObjectId;

use crate::git::CommitRow;

#[derive(Clone)]
pub struct GraphRow {
    pub commit_lane: usize,
    pub incoming: Vec<Option<ObjectId>>, // lane state above this row
    pub outgoing: Vec<Option<ObjectId>>, // lane state below this row
    /// Lane indices that merged INTO commit_lane at this row (their downward
    /// line ends here, joining the commit).
    pub merged_in: Vec<usize>,
    /// Lane indices that branched OUT of commit_lane at this row (extra
    /// parents — their downward line begins here).
    pub branched_out: Vec<usize>,
}

pub fn build(commits: &[CommitRow]) -> Vec<GraphRow> {
    let mut rows = Vec::with_capacity(commits.len());
    let mut active: Vec<Option<ObjectId>> = Vec::new();

    for commit in commits {
        let incoming = active.clone();

        // Find lanes waiting for this commit.
        let mut my_lane: Option<usize> = None;
        let mut merged_in: Vec<usize> = Vec::new();
        for (idx, slot) in active.iter().enumerate() {
            if slot.as_ref() == Some(&commit.id) {
                if my_lane.is_none() {
                    my_lane = Some(idx);
                } else {
                    merged_in.push(idx);
                }
            }
        }
        let my_lane = my_lane.unwrap_or_else(|| {
            if let Some(idx) = active.iter().position(|s| s.is_none()) {
                idx
            } else {
                active.push(None);
                active.len() - 1
            }
        });

        // Clear merged-in lanes — they collapse here.
        for &l in &merged_in {
            active[l] = None;
        }

        // Wire up parents.
        let mut branched_out: Vec<usize> = Vec::new();
        if commit.parents.is_empty() {
            active[my_lane] = None;
        } else {
            active[my_lane] = Some(commit.parents[0]);
            for p in &commit.parents[1..] {
                let new_lane = if let Some(idx) = active.iter().position(|s| s.is_none()) {
                    idx
                } else {
                    active.push(None);
                    active.len() - 1
                };
                active[new_lane] = Some(*p);
                branched_out.push(new_lane);
            }
        }

        while matches!(active.last(), Some(None)) {
            active.pop();
        }

        rows.push(GraphRow {
            commit_lane: my_lane,
            incoming,
            outgoing: active.clone(),
            merged_in,
            branched_out,
        });
    }

    rows
}

/// A pre-rendered cell. The `usize` carries the lane index for coloring.
#[derive(Clone, Copy)]
pub enum Cell {
    Empty,
    Vertical(usize),  // │
    Commit(usize),    // ●
    Horizontal,       // ─
    Cross(usize),     // ┼  (vertical lane crossing a horizontal connector)
    CornerUL(usize),  // ╯  top + left  (merge in from the right)
    CornerUR(usize),  // ╰  top + right (merge in from the left)
    CornerDL(usize),  // ╮  bottom + left (branch out to the right)
    CornerDR(usize),  // ╭  bottom + right (branch out to the left)
}

/// Lay out the cells for a single commit row.
pub fn render_row(row: &GraphRow) -> Vec<Cell> {
    let max_lane = row
        .incoming
        .len()
        .max(row.outgoing.len())
        .max(row.commit_lane + 1)
        .max(
            row.merged_in
                .iter()
                .chain(row.branched_out.iter())
                .copied()
                .max()
                .map(|m| m + 1)
                .unwrap_or(0),
        );

    let mut cells: Vec<Cell> = (0..max_lane).map(|_| Cell::Empty).collect();

    // 1. Lay down vertical bars for any lane active in BOTH incoming and outgoing.
    for lane in 0..max_lane {
        let in_active = row.incoming.get(lane).and_then(|s| s.as_ref()).is_some();
        let out_active = row.outgoing.get(lane).and_then(|s| s.as_ref()).is_some();
        if in_active && out_active {
            cells[lane] = Cell::Vertical(lane);
        }
    }

    // 2. Merged-in lanes: at the merge column, place a corner that points
    //    toward the commit lane. Fill the cells between with horizontal
    //    connectors (turning verticals into crossings).
    for &m in &row.merged_in {
        let corner = if m > row.commit_lane {
            Cell::CornerUL(m) // top + left
        } else {
            Cell::CornerUR(m) // top + right
        };
        cells[m] = corner;
        connect_horizontally(&mut cells, row.commit_lane, m);
    }

    // 3. Branched-out lanes: corner points down toward the new lane.
    for &b in &row.branched_out {
        let corner = if b > row.commit_lane {
            Cell::CornerDL(b) // bottom + left
        } else {
            Cell::CornerDR(b) // bottom + right
        };
        cells[b] = corner;
        connect_horizontally(&mut cells, row.commit_lane, b);
    }

    // 4. The commit dot itself — placed last so it overrides anything else.
    cells[row.commit_lane] = Cell::Commit(row.commit_lane);

    cells
}

fn connect_horizontally(cells: &mut [Cell], from: usize, to: usize) {
    if from == to {
        return;
    }
    let (lo, hi) = if from < to { (from, to) } else { (to, from) };
    for lane in (lo + 1)..hi {
        cells[lane] = match cells[lane] {
            Cell::Vertical(l) => Cell::Cross(l),
            Cell::Empty => Cell::Horizontal,
            // Don't overwrite an existing corner/cross/etc — leave it.
            other => other,
        };
    }
}
