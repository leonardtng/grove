use anyhow::Result;
use gix::object::tree::diff::change::Event;
use gix::object::tree::diff::Action;
use gix::ObjectId;

#[derive(Clone)]
pub struct FileChange {
    pub status: ChangeStatus,
    pub path: String,
}

#[derive(Clone, Copy)]
pub enum ChangeStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl ChangeStatus {
    pub fn letter(self) -> char {
        match self {
            ChangeStatus::Added => 'A',
            ChangeStatus::Deleted => 'D',
            ChangeStatus::Modified => 'M',
            ChangeStatus::Renamed => 'R',
        }
    }
}

/// Compute the file changes introduced by `commit_id` (vs its first parent,
/// or vs the empty tree for root commits).
pub fn changes_for_commit(repo: &gix::Repository, commit_id: ObjectId) -> Result<Vec<FileChange>> {
    let commit = repo.find_commit(commit_id)?;
    let new_tree = commit.tree()?;

    let parent_tree = match commit.parent_ids().next() {
        Some(parent_id) => repo.find_commit(parent_id.detach())?.tree()?,
        None => repo.empty_tree(),
    };

    let mut out: Vec<FileChange> = Vec::new();
    let mut platform = parent_tree.changes()?;
    platform.track_path();
    platform.for_each_to_obtain_tree(&new_tree, |change| {
        // Skip directory (tree) entries — gix yields one for every changed
        // sub-tree along the way as well as the actual blob. We only want
        // file-level changes.
        let mode = match change.event {
            Event::Addition { entry_mode, .. } => entry_mode,
            Event::Deletion { entry_mode, .. } => entry_mode,
            Event::Modification { entry_mode, .. } => entry_mode,
            Event::Rewrite { entry_mode, .. } => entry_mode,
        };
        if mode.is_tree() {
            return Ok::<_, std::convert::Infallible>(Action::Continue);
        }

        let path = change.location.to_string();
        let status = match change.event {
            Event::Addition { .. } => ChangeStatus::Added,
            Event::Deletion { .. } => ChangeStatus::Deleted,
            Event::Modification { .. } => ChangeStatus::Modified,
            Event::Rewrite { .. } => ChangeStatus::Renamed,
        };
        out.push(FileChange { status, path });
        Ok::<_, std::convert::Infallible>(Action::Continue)
    })?;

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
