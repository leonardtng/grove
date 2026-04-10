//! Inline (whole-file) diff viewer for a single file change.
//!
//! Renders the entire `new` version of the file with `+`/`-` annotations
//! interleaved at the change sites — the same view IDEs use for inline
//! diffs. For an Addition the entire file is `+`; for a Deletion the
//! entire `old` file is `-`.

use anyhow::Result;
use gix::ObjectId;
use imara_diff::intern::InternedInput;
use imara_diff::{diff, Algorithm, Sink};
use std::ops::Range;
use std::path::Path;

use crate::diff::ChangeStatus;
use crate::syntax::{self, HlSpan};

#[derive(Clone, Copy)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
}

pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    /// 1-based line number in the new file (None for deleted-only lines).
    pub new_no: Option<u32>,
    /// 1-based line number in the old file (None for added-only lines).
    pub old_no: Option<u32>,
}

pub struct FileDiff {
    pub path: String,
    pub status: ChangeStatus,
    pub lines: Vec<DiffLine>,
    pub scroll: u16,
    /// Highlighted spans for the new-file content, indexed by (new_line - 1).
    pub hl_new: Vec<Vec<HlSpan>>,
    /// Highlighted spans for the old-file content, indexed by (old_line - 1).
    pub hl_old: Vec<Vec<HlSpan>>,
    pub language: String,
}

struct HunkCollector {
    hunks: Vec<(Range<u32>, Range<u32>)>,
}

impl Sink for HunkCollector {
    type Out = Vec<(Range<u32>, Range<u32>)>;
    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        self.hunks.push((before, after));
    }
    fn finish(self) -> Self::Out {
        self.hunks
    }
}

impl FileDiff {
    pub fn compute(
        repo: &gix::Repository,
        commit_id: ObjectId,
        path: &str,
        status: ChangeStatus,
    ) -> Result<Self> {
        let (old_bytes, new_bytes) = extract_blob_pair(repo, commit_id, path)?;
        let old_text = String::from_utf8_lossy(&old_bytes).into_owned();
        let new_text = String::from_utf8_lossy(&new_bytes).into_owned();

        let lines = build_inline(&old_text, &new_text);
        let new_hl = syntax::highlight(&new_text, path);
        let old_hl = syntax::highlight(&old_text, path);
        let language = if !new_hl.language.is_empty() {
            new_hl.language.clone()
        } else {
            old_hl.language.clone()
        };

        Ok(Self {
            path: path.to_string(),
            status,
            lines,
            scroll: 0,
            hl_new: new_hl.lines,
            hl_old: old_hl.lines,
            language,
        })
    }

    pub fn scroll_by(&mut self, delta: i32, _page: u16) {
        let max = self.lines.len().saturating_sub(1) as i32;
        let next = (self.scroll as i32 + delta).clamp(0, max);
        self.scroll = next as u16;
    }
}

fn build_inline(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = split_lines(old_text);
    let new_lines: Vec<&str> = split_lines(new_text);

    let input = InternedInput::new(old_text, new_text);
    let hunks = diff(
        Algorithm::Histogram,
        &input,
        HunkCollector { hunks: Vec::new() },
    );

    let mut out: Vec<DiffLine> = Vec::new();
    let mut old_pos: u32 = 0;
    let mut new_pos: u32 = 0;

    for (before, after) in hunks {
        // Walk forward through unchanged context up to this hunk.
        while new_pos < after.start {
            out.push(DiffLine {
                kind: LineKind::Context,
                text: new_lines
                    .get(new_pos as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                new_no: Some(new_pos + 1),
                old_no: Some(old_pos + 1),
            });
            new_pos += 1;
            old_pos += 1;
        }

        // Removed lines come from the OLD file.
        for i in before.clone() {
            out.push(DiffLine {
                kind: LineKind::Deletion,
                text: old_lines
                    .get(i as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                new_no: None,
                old_no: Some(i + 1),
            });
        }
        // Added lines come from the NEW file.
        for i in after.clone() {
            out.push(DiffLine {
                kind: LineKind::Addition,
                text: new_lines
                    .get(i as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                new_no: Some(i + 1),
                old_no: None,
            });
        }
        old_pos = before.end;
        new_pos = after.end;
    }

    // Trailing context after the last hunk.
    while (new_pos as usize) < new_lines.len() {
        out.push(DiffLine {
            kind: LineKind::Context,
            text: new_lines[new_pos as usize].to_string(),
            new_no: Some(new_pos + 1),
            old_no: Some(old_pos + 1),
        });
        new_pos += 1;
        old_pos += 1;
    }

    if out.is_empty() {
        out.push(DiffLine {
            kind: LineKind::Context,
            text: "(no textual differences)".to_string(),
            new_no: None,
            old_no: None,
        });
    }

    out
}

fn split_lines(s: &str) -> Vec<&str> {
    // Preserve line count for files without a trailing newline.
    if s.is_empty() {
        return Vec::new();
    }
    s.split('\n').collect::<Vec<_>>().into_iter().collect()
}

fn extract_blob_pair(
    repo: &gix::Repository,
    commit_id: ObjectId,
    path: &str,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let commit = repo.find_commit(commit_id)?;
    let new_tree = commit.tree()?;
    let new_bytes = blob_at_path(repo, &new_tree, path).unwrap_or_default();

    let old_bytes = if let Some(parent_id) = commit.parent_ids().next() {
        let parent_tree = repo.find_commit(parent_id.detach())?.tree()?;
        blob_at_path(repo, &parent_tree, path).unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok((old_bytes, new_bytes))
}

fn blob_at_path(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    path: &str,
) -> Option<Vec<u8>> {
    let entry = tree
        .clone()
        .peel_to_entry_by_path(Path::new(path))
        .ok()??;
    let object = repo.find_object(entry.object_id()).ok()?;
    Some(object.data.clone())
}
