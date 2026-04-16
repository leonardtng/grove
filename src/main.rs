use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{collections::HashMap, io, path::PathBuf};

mod diff;
mod diffview;
mod git;
mod graph;
mod syntax;

use diff::{ChangeStatus, FileChange};
use diffview::{DiffLine, FileDiff, LineKind};
use git::{CommitRow, LoadedRepo, RefKind, RefLabel};
use graph::{Cell, GraphRow};
use syntax::HlSpan;

const LANE_COLORS: &[Color] = &[
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Green,
    Color::Blue,
    Color::Red,
    Color::LightCyan,
    Color::LightMagenta,
];

struct App {
    repo_path: PathBuf,
    limit: usize,
    loaded: LoadedRepo,
    graph_rows: Vec<GraphRow>,
    selected: Option<usize>,
    list_scroll: u16,
    list_view_height: u16,
    expanded: Vec<bool>,
    file_cache: HashMap<usize, Vec<FileChange>>,
    diff_view: Option<FileDiff>,
    status: String,
    toolbar_buttons: Vec<(u16, u16, ToolbarAction)>, // (start_x, end_x, action)
    pending: Option<PendingAction>,
    input: Option<InputState>,
    picker: Option<Picker>,
    dirty: bool,
    uncommitted_expanded: bool,
    uncommitted_files: Option<Vec<FileChange>>,
    /// Position of the "changes only / full file" toggle in the diff panel
    /// title, captured each frame for click dispatch. (row, x_start, x_end).
    diff_toggle_button: Option<(u16, u16, u16)>,
    should_quit: bool,
}

#[derive(Clone, Copy)]
enum ToolbarAction {
    Refresh,
    Fetch,
    Pull,
    Tag,
    PushTags,
}

#[derive(Clone)]
enum PendingAction {
    Refresh,
    Fetch,
    Pull,
    CreateTag { commit_id: gix::ObjectId, name: String },
    PushTags,
    CreateBranch { commit_id: gix::ObjectId, name: String },
    CheckoutBranch { name: String },
    CheckoutCommit { sha: String },
    DeleteBranch { name: String },
    RenameBranch { old: String, new: String },
}

struct InputState {
    prompt: String,
    buffer: String,
    kind: InputKind,
}

#[derive(Clone)]
enum InputKind {
    TagName,
    BranchName { commit_id: gix::ObjectId },
    RenameBranch { old: String },
}

struct Picker {
    title: String,
    items: Vec<String>,
    selected: usize,
    kind: PickerKind,
}

#[derive(Clone, Copy)]
enum PickerKind {
    Checkout,
    Delete,
}

impl App {
    fn new(repo_path: PathBuf, limit: usize) -> Result<Self> {
        let loaded = git::load_repo(&repo_path, limit)?;
        let graph_rows = graph::build(&loaded.commits);
        let expanded = vec![false; loaded.commits.len()];
        let selected = if loaded.commits.is_empty() { None } else { Some(0) };
        let status = format!("loaded {} commits", loaded.commits.len());
        let mut app = Self {
            repo_path,
            limit,
            loaded,
            graph_rows,
            selected,
            list_scroll: 0,
            list_view_height: 0,
            expanded,
            file_cache: HashMap::new(),
            diff_view: None,
            status,
            toolbar_buttons: Vec::new(),
            pending: None,
            input: None,
            picker: None,
            dirty: false,
            uncommitted_expanded: false,
            uncommitted_files: None,
            diff_toggle_button: None,
            should_quit: false,
        };
        app.detect_dirty();
        Ok(app)
    }

    fn detect_dirty(&mut self) {
        let work_dir = self
            .loaded
            .repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_path.clone());
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&work_dir)
            .output();
        self.dirty = matches!(out, Ok(o) if o.status.success() && !o.stdout.is_empty());
        // Invalidate the cached file list — must reload on next expand.
        self.uncommitted_files = None;
        if !self.dirty {
            self.uncommitted_expanded = false;
        }
    }

    fn load_uncommitted_files(&mut self) {
        let work_dir = self
            .loaded
            .repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_path.clone());
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&work_dir)
            .output();
        let mut files: Vec<FileChange> = Vec::new();
        if let Ok(o) = out {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                for line in stdout.lines() {
                    if line.len() < 3 {
                        continue;
                    }
                    let staged = line.chars().next().unwrap_or(' ');
                    let unstaged = line.chars().nth(1).unwrap_or(' ');
                    // For renames/copies the line format is "R  old -> new".
                    let path_part = &line[3..];
                    let path = if let Some(idx) = path_part.find(" -> ") {
                        path_part[idx + 4..].to_string()
                    } else {
                        path_part.to_string()
                    };
                    let status = if staged == 'D' || unstaged == 'D' {
                        ChangeStatus::Deleted
                    } else if staged == 'A' || (staged == '?' && unstaged == '?') {
                        ChangeStatus::Added
                    } else if staged == 'R' {
                        ChangeStatus::Renamed
                    } else {
                        ChangeStatus::Modified
                    };
                    files.push(FileChange { status, path });
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        self.uncommitted_files = Some(files);
    }

    fn toggle_uncommitted_expand(&mut self) {
        if !self.dirty {
            return;
        }
        self.uncommitted_expanded = !self.uncommitted_expanded;
        if self.uncommitted_expanded && self.uncommitted_files.is_none() {
            self.load_uncommitted_files();
        }
        self.clamp_scroll();
    }

    fn open_uncommitted_file(&mut self, file_idx: usize) {
        let Some(files) = &self.uncommitted_files else { return };
        let Some(fc) = files.get(file_idx).cloned() else { return };
        let work_dir = self
            .loaded
            .repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_path.clone());
        match FileDiff::compute_uncommitted(&self.loaded.repo, &work_dir, &fc.path, fc.status) {
            Ok(view) => self.diff_view = Some(view),
            Err(e) => self.status = format!("diff failed: {e}"),
        }
    }

    /// Number of lines the virtual "uncommitted" block occupies (commit row +
    /// expanded files).
    fn virtual_height(&self) -> usize {
        if !self.dirty {
            return 0;
        }
        if self.uncommitted_expanded {
            let n = self.uncommitted_files.as_ref().map(|v| v.len()).unwrap_or(0);
            1 + n
        } else {
            1
        }
    }

    fn virtual_offset(&self) -> usize {
        self.virtual_height()
    }

    fn queue_refresh(&mut self) {
        self.status = "refreshing...".to_string();
        self.pending = Some(PendingAction::Refresh);
    }

    fn queue_fetch(&mut self) {
        self.status = "fetching from remotes...".to_string();
        self.pending = Some(PendingAction::Fetch);
    }

    fn queue_pull(&mut self) {
        self.status = "pulling (--ff-only)...".to_string();
        self.pending = Some(PendingAction::Pull);
    }

    fn queue_push_tags(&mut self) {
        self.status = "pushing tags to origin...".to_string();
        self.pending = Some(PendingAction::PushTags);
    }

    fn start_tag_input(&mut self) {
        if self.selected.is_none() || self.loaded.commits.is_empty() {
            self.status = "no commit selected".to_string();
            return;
        }
        self.input = Some(InputState {
            prompt: "tag name: ".to_string(),
            buffer: String::new(),
            kind: InputKind::TagName,
        });
    }

    fn cancel_input(&mut self) {
        self.input = None;
    }

    fn submit_input(&mut self) {
        let Some(input) = self.input.take() else { return };
        let value = input.buffer.trim().to_string();
        if value.is_empty() {
            self.status = "input was empty".to_string();
            return;
        }
        match input.kind {
            InputKind::TagName => {
                let Some(idx) = self.selected else { return };
                let Some(commit) = self.loaded.commits.get(idx) else { return };
                let commit_id = commit.id;
                self.status = format!("creating tag '{value}'...");
                self.pending = Some(PendingAction::CreateTag { commit_id, name: value });
            }
            InputKind::BranchName { commit_id } => {
                self.status = format!("creating branch '{value}'...");
                self.pending = Some(PendingAction::CreateBranch { commit_id, name: value });
            }
            InputKind::RenameBranch { old } => {
                self.status = format!("renaming '{old}' → '{value}'...");
                self.pending = Some(PendingAction::RenameBranch { old, new: value });
            }
        }
    }

    fn input_char(&mut self, c: char) {
        if let Some(input) = &mut self.input {
            input.buffer.push(c);
        }
    }

    fn input_backspace(&mut self) {
        if let Some(input) = &mut self.input {
            input.buffer.pop();
        }
    }

    fn create_tag(&mut self, commit_id: gix::ObjectId, name: &str) {
        let sha = commit_id.to_hex().to_string();
        self.run_git(&["tag", name, &sha], &format!("tag '{name}'"));
    }

    fn push_tags(&mut self) {
        self.run_git(&["push", "origin", "--tags"], "push tags");
    }

    fn local_branches_at(&self, commit_idx: usize) -> Vec<String> {
        let Some(commit) = self.loaded.commits.get(commit_idx) else { return Vec::new() };
        let Some(labels) = self.loaded.refs_by_id.get(&commit.id) else { return Vec::new() };
        labels
            .iter()
            .filter(|l| matches!(l.kind, RefKind::LocalBranch))
            .map(|l| l.name.clone())
            .collect()
    }

    fn start_branch_input(&mut self) {
        let Some(idx) = self.selected else {
            self.status = "no commit selected".to_string();
            return;
        };
        let Some(commit) = self.loaded.commits.get(idx) else { return };
        self.input = Some(InputState {
            prompt: "branch name: ".to_string(),
            buffer: String::new(),
            kind: InputKind::BranchName { commit_id: commit.id },
        });
    }

    fn checkout_selected(&mut self) {
        let Some(idx) = self.selected else {
            self.status = "no commit selected".to_string();
            return;
        };
        let branches = self.local_branches_at(idx);
        match branches.len() {
            0 => {
                // No local branch — detach.
                let Some(commit) = self.loaded.commits.get(idx) else { return };
                let sha = commit.id_hex.clone();
                let short = sha[..7].to_string();
                self.status = format!("checking out commit {short} (detached)...");
                self.pending = Some(PendingAction::CheckoutCommit { sha });
            }
            1 => {
                let name = branches.into_iter().next().unwrap();
                self.status = format!("checking out branch '{name}'...");
                self.pending = Some(PendingAction::CheckoutBranch { name });
            }
            _ => {
                self.picker = Some(Picker {
                    title: "checkout which branch?".to_string(),
                    items: branches,
                    selected: 0,
                    kind: PickerKind::Checkout,
                });
            }
        }
    }

    fn delete_branch_at_selected(&mut self) {
        let Some(idx) = self.selected else {
            self.status = "no commit selected".to_string();
            return;
        };
        let branches = self.local_branches_at(idx);
        match branches.len() {
            0 => {
                self.status = "no local branch on this commit".to_string();
            }
            1 => {
                let name = branches.into_iter().next().unwrap();
                self.status = format!("deleting branch '{name}'...");
                self.pending = Some(PendingAction::DeleteBranch { name });
            }
            _ => {
                self.picker = Some(Picker {
                    title: "delete which branch?".to_string(),
                    items: branches,
                    selected: 0,
                    kind: PickerKind::Delete,
                });
            }
        }
    }

    fn picker_move(&mut self, delta: i32) {
        if let Some(p) = &mut self.picker {
            let len = p.items.len() as i32;
            if len == 0 {
                return;
            }
            let next = (p.selected as i32 + delta).rem_euclid(len);
            p.selected = next as usize;
        }
    }

    fn picker_cancel(&mut self) {
        self.picker = None;
    }

    fn picker_submit(&mut self) {
        let Some(p) = self.picker.take() else { return };
        let Some(name) = p.items.get(p.selected).cloned() else { return };
        match p.kind {
            PickerKind::Checkout => {
                self.status = format!("checking out branch '{name}'...");
                self.pending = Some(PendingAction::CheckoutBranch { name });
            }
            PickerKind::Delete => {
                self.status = format!("deleting branch '{name}'...");
                self.pending = Some(PendingAction::DeleteBranch { name });
            }
        }
    }

    fn start_rename_input(&mut self) {
        // Read current HEAD branch via git itself — authoritative.
        let work_dir = self
            .loaded
            .repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_path.clone());
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&work_dir)
            .output();
        let name = match out {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            }
            _ => String::new(),
        };
        if name.is_empty() || name == "HEAD" {
            self.status = "not on a branch".to_string();
            return;
        }
        self.input = Some(InputState {
            prompt: format!("rename '{name}' to: "),
            buffer: String::new(),
            kind: InputKind::RenameBranch { old: name },
        });
    }

    fn checkout_branch(&mut self, name: &str) {
        self.run_git(&["checkout", name], &format!("checkout '{name}'"));
    }

    fn checkout_commit(&mut self, sha: &str) {
        self.run_git(&["checkout", sha], &format!("checkout {}", &sha[..7]));
    }

    fn create_branch(&mut self, commit_id: gix::ObjectId, name: &str) {
        let sha = commit_id.to_hex().to_string();
        self.run_git(&["branch", name, &sha], &format!("branch '{name}'"));
    }

    fn delete_branch(&mut self, name: &str) {
        self.run_git(&["branch", "-D", name], &format!("delete '{name}'"));
    }

    fn rename_branch(&mut self, old: &str, new: &str) {
        self.run_git(&["branch", "-m", old, new], &format!("rename → '{new}'"));
    }

    fn refresh(&mut self) {
        match git::load_repo(&self.repo_path, self.limit) {
            Ok(loaded) => {
                self.graph_rows = graph::build(&loaded.commits);
                self.expanded = vec![false; loaded.commits.len()];
                self.file_cache.clear();
                if let Some(s) = self.selected {
                    if s >= loaded.commits.len() {
                        self.selected = if loaded.commits.is_empty() { None } else { Some(0) };
                    }
                }
                self.status = format!("refreshed: {} commits", loaded.commits.len());
                self.loaded = loaded;
                self.clamp_scroll();
                self.detect_dirty();
            }
            Err(e) => {
                self.status = format!("refresh failed: {e}");
            }
        }
    }

    fn run_git(&mut self, args: &[&str], label: &str) {
        let work_dir = self
            .loaded
            .repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.repo_path.clone());
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&work_dir)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                self.status = format!("{label} ok");
                self.refresh();
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                self.status = format!("{label} failed: {}", msg.lines().next().unwrap_or(""));
            }
            Err(e) => {
                self.status = format!("{label} error: {e}");
            }
        }
    }

    fn fetch(&mut self) {
        self.status = "fetching...".to_string();
        self.run_git(&["fetch", "--all", "--prune"], "fetch");
    }

    fn pull(&mut self) {
        self.status = "pulling...".to_string();
        self.run_git(&["pull", "--ff-only"], "pull");
    }

    fn selected_idx(&self) -> Option<usize> {
        self.selected
    }

    fn selected_commit(&self) -> Option<&CommitRow> {
        self.selected_idx().and_then(|i| self.loaded.commits.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.loaded.commits.is_empty() {
            return;
        }
        let len = self.loaded.commits.len() as isize;
        let cur = self.selected.unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        self.selected = Some(next);
        self.ensure_selection_visible();
    }

    /// Total height in lines if all items are stacked.
    fn total_lines(&self) -> usize {
        self.virtual_offset() + (0..self.loaded.commits.len()).map(|i| self.item_height(i)).sum::<usize>()
    }

    /// First visible line index of the commit at `idx`, given current expansion state.
    fn line_offset_of(&self, idx: usize) -> usize {
        self.virtual_offset() + (0..idx).map(|i| self.item_height(i)).sum::<usize>()
    }

    fn ensure_selection_visible(&mut self) {
        let Some(sel) = self.selected else { return };
        let first = self.line_offset_of(sel) as i32;
        let last = first + self.item_height(sel) as i32 - 1;
        let scroll = self.list_scroll as i32;
        let view_h = self.list_view_height.max(1) as i32;
        if first < scroll {
            self.list_scroll = first.max(0) as u16;
        } else if last >= scroll + view_h {
            self.list_scroll = (last - view_h + 1).max(0) as u16;
        }
    }

    fn clamp_scroll(&mut self) {
        let total = self.total_lines() as i32;
        let view_h = self.list_view_height.max(1) as i32;
        let max = (total - view_h).max(0);
        if (self.list_scroll as i32) > max {
            self.list_scroll = max as u16;
        }
    }

    fn toggle_expand(&mut self) {
        let Some(idx) = self.selected_idx() else { return };
        self.expanded[idx] = !self.expanded[idx];
        if self.expanded[idx] && !self.file_cache.contains_key(&idx) {
            let id = self.loaded.commits[idx].id;
            match diff::changes_for_commit(&self.loaded.repo, id) {
                Ok(changes) => {
                    self.file_cache.insert(idx, changes);
                }
                Err(_) => {
                    self.file_cache.insert(idx, Vec::new());
                }
            }
        }
        self.clamp_scroll();
    }

    fn open_selected_file(&mut self, commit_idx: usize, file_idx: usize) {
        let Some(files) = self.file_cache.get(&commit_idx) else { return };
        let Some(fc) = files.get(file_idx).cloned() else { return };
        let id = self.loaded.commits[commit_idx].id;
        // Show the diff inline in our own viewer panel.
        match FileDiff::compute(&self.loaded.repo, id, &fc.path, fc.status) {
            Ok(view) => self.diff_view = Some(view),
            Err(_) => {}
        }
    }

    fn close_diff_view(&mut self) {
        self.diff_view = None;
    }

    fn scroll_diff(&mut self, delta: i32) {
        if let Some(v) = &mut self.diff_view {
            v.scroll_by(delta, 1);
        }
    }

    fn handle_mouse(
        &mut self,
        ev: MouseEvent,
        toolbar_row: u16,
        list_area: Rect,
        right_area: Rect,
    ) {
        // Toolbar button clicks.
        if matches!(ev.kind, MouseEventKind::Down(_)) && ev.row == toolbar_row {
            let buttons = self.toolbar_buttons.clone();
            for (start, end, action) in buttons {
                if ev.column >= start && ev.column < end {
                    match action {
                        ToolbarAction::Refresh => self.queue_refresh(),
                        ToolbarAction::Fetch => self.queue_fetch(),
                        ToolbarAction::Pull => self.queue_pull(),
                        ToolbarAction::Tag => self.start_tag_input(),
                        ToolbarAction::PushTags => self.queue_push_tags(),
                    }
                    return;
                }
            }
        }

        // Diff panel's "changes only / full file" toggle (in the top border).
        if matches!(ev.kind, MouseEventKind::Down(_)) {
            if let Some((row, x_start, x_end)) = self.diff_toggle_button {
                if ev.row == row && ev.column >= x_start && ev.column < x_end {
                    if let Some(v) = &mut self.diff_view {
                        v.toggle_only_changes();
                    }
                    return;
                }
            }
        }

        let in_list = point_in(ev.column, ev.row, list_area);
        let in_right = point_in(ev.column, ev.row, right_area);

        match ev.kind {
            MouseEventKind::ScrollDown => {
                if in_right && self.diff_view.is_some() {
                    self.scroll_diff(3);
                } else if in_list {
                    self.scroll_list(3);
                }
                return;
            }
            MouseEventKind::ScrollUp => {
                if in_right && self.diff_view.is_some() {
                    self.scroll_diff(-3);
                } else if in_list {
                    self.scroll_list(-3);
                }
                return;
            }
            MouseEventKind::Down(_) => {}
            _ => return,
        }

        if !in_list {
            return;
        }
        // The list block has a 1-row top border.
        let inner_top = list_area.y + 1;
        if ev.row < inner_top {
            return;
        }
        let row_in_view = (ev.row - inner_top) as usize;

        // Map (row_in_view + scroll) → which commit + sub-row. The virtual
        // "uncommitted changes" block (if dirty) lives at lines 0..virtual_height.
        let target_line = self.list_scroll as usize + row_in_view;
        let vh = self.virtual_offset();
        if target_line < vh {
            // Inside the virtual block.
            if target_line == 0 {
                self.toggle_uncommitted_expand();
            } else {
                let file_idx = target_line - 1;
                self.open_uncommitted_file(file_idx);
            }
            return;
        }
        let target_line = target_line - vh;
        let mut acc = 0usize;
        for idx in 0..self.loaded.commits.len() {
            let h = self.item_height(idx);
            if target_line < acc + h {
                let sub = target_line - acc;
                self.selected = Some(idx);
                if sub == 0 {
                    self.toggle_expand();
                } else {
                    self.open_selected_file(idx, sub - 1);
                }
                return;
            }
            acc += h;
        }
    }

    /// Pure offset scroll — does not move the selection.
    fn scroll_list(&mut self, delta: i32) {
        if self.loaded.commits.is_empty() {
            return;
        }
        let total = self.total_lines() as i32;
        let view_h = self.list_view_height.max(1) as i32;
        let max = (total - view_h).max(0);
        let next = (self.list_scroll as i32 + delta).clamp(0, max);
        self.list_scroll = next as u16;
    }

    fn item_height(&self, idx: usize) -> usize {
        if self.expanded[idx] {
            let n = self.file_cache.get(&idx).map(|v| v.len()).unwrap_or(0);
            1 + n
        } else {
            1
        }
    }
}

fn main() -> Result<()> {
    let (repo_path, limit) = parse_args();
    let mut app = App::new(repo_path, limit)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    let mut last_list_area = Rect::default();
    let mut last_right_area = Rect::default();
    let mut last_toolbar_row: u16 = 0;

    while !app.should_quit {
        terminal.draw(|f| {
            let (tb, l, r) = draw(f, app);
            last_toolbar_row = tb;
            last_list_area = l;
            last_right_area = r;
        })?;

        // Run any pending action AFTER the draw so the "refreshing..." /
        // "fetching..." status is visible while the (blocking) git command runs.
        if let Some(action) = app.pending.take() {
            match action {
                PendingAction::Refresh => app.refresh(),
                PendingAction::Fetch => app.fetch(),
                PendingAction::Pull => app.pull(),
                PendingAction::CreateTag { commit_id, name } => {
                    app.create_tag(commit_id, &name);
                }
                PendingAction::PushTags => app.push_tags(),
                PendingAction::CreateBranch { commit_id, name } => {
                    app.create_branch(commit_id, &name);
                }
                PendingAction::CheckoutBranch { name } => app.checkout_branch(&name),
                PendingAction::CheckoutCommit { sha } => app.checkout_commit(&sha),
                PendingAction::DeleteBranch { name } => app.delete_branch(&name),
                PendingAction::RenameBranch { old, new } => app.rename_branch(&old, &new),
            }
            continue; // re-render with the result status before reading input
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Picker mode swallows everything except nav/Esc/Enter.
                if app.picker.is_some() {
                    match key.code {
                        KeyCode::Esc => app.picker_cancel(),
                        KeyCode::Enter => app.picker_submit(),
                        KeyCode::Down | KeyCode::Char('j') => app.picker_move(1),
                        KeyCode::Up | KeyCode::Char('k') => app.picker_move(-1),
                        _ => {}
                    }
                    continue;
                }
                // Input mode swallows everything except Esc/Enter/typing.
                if app.input.is_some() {
                    match key.code {
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Enter => app.submit_input(),
                        KeyCode::Backspace => app.input_backspace(),
                        KeyCode::Char(c) => app.input_char(c),
                        _ => {}
                    }
                    continue;
                }
                let in_diff = app.diff_view.is_some();
                // Keys common to both modes.
                match key.code {
                    KeyCode::Char('q') => {
                        app.should_quit = true;
                        continue;
                    }
                    KeyCode::Esc => {
                        if in_diff {
                            app.close_diff_view();
                        } else {
                            app.should_quit = true;
                        }
                        continue;
                    }
                    _ => {}
                }
                if in_diff {
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => app.scroll_diff(1),
                        KeyCode::Up | KeyCode::Char('k') => app.scroll_diff(-1),
                        KeyCode::PageDown => app.scroll_diff(20),
                        KeyCode::PageUp => app.scroll_diff(-20),
                        KeyCode::Home => {
                            if let Some(v) = &mut app.diff_view {
                                v.scroll = 0;
                            }
                        }
                        KeyCode::End => {
                            if let Some(v) = &mut app.diff_view {
                                v.scroll = v.lines.len().saturating_sub(1) as u16;
                            }
                        }
                        KeyCode::Char('c') => {
                            if let Some(v) = &mut app.diff_view {
                                v.toggle_only_changes();
                            }
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                        KeyCode::PageDown => app.move_selection(20),
                        KeyCode::PageUp => app.move_selection(-20),
                        KeyCode::Home => {
                            app.selected = Some(0);
                            app.list_scroll = 0;
                        }
                        KeyCode::End => {
                            if !app.loaded.commits.is_empty() {
                                app.selected = Some(app.loaded.commits.len() - 1);
                                app.ensure_selection_visible();
                            }
                        }
                        KeyCode::Enter | KeyCode::Char(' ') => app.toggle_expand(),
                        KeyCode::Char('r') => app.queue_refresh(),
                        KeyCode::Char('f') => app.queue_fetch(),
                        KeyCode::Char('p') => app.queue_pull(),
                        KeyCode::Char('t') => app.start_tag_input(),
                        KeyCode::Char('T') => app.queue_push_tags(),
                        KeyCode::Char('b') => app.start_branch_input(),
                        KeyCode::Char('c') => app.checkout_selected(),
                        KeyCode::Char('D') => app.delete_branch_at_selected(),
                        KeyCode::Char('n') => app.start_rename_input(),
                        _ => {}
                    }
                }
            }
            Event::Mouse(m) => {
                app.handle_mouse(m, last_toolbar_row, last_list_area, last_right_area)
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

fn parse_args() -> (PathBuf, usize) {
    let mut repo: Option<PathBuf> = None;
    let mut limit: usize = 10_000;
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--limit" | "-n" => {
                if let Some(v) = iter.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        limit = n;
                    }
                }
            }
            other if other.starts_with("--limit=") => {
                if let Ok(n) = other.trim_start_matches("--limit=").parse::<usize>() {
                    limit = n;
                }
            }
            other => {
                if repo.is_none() {
                    repo = Some(PathBuf::from(other));
                }
            }
        }
    }
    (repo.unwrap_or_else(|| PathBuf::from(".")), limit)
}

fn point_in(col: u16, row: u16, area: Rect) -> bool {
    col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
}

fn draw(f: &mut Frame, app: &mut App) -> (u16, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // toolbar
            Constraint::Min(1),    // main
            Constraint::Length(1), // help
        ])
        .split(f.area());

    let toolbar_area = chunks[0];
    render_toolbar(f, toolbar_area, app);

    let in_diff = app.diff_view.is_some();
    let (left_pct, right_pct) = if in_diff { (40u16, 60u16) } else { (65u16, 35u16) };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(left_pct), Constraint::Percentage(right_pct)])
        .split(chunks[1]);

    let list_area = cols[0];
    let right_area = cols[1];

    // Compute the visible inner height (subtract block borders) and store it
    // so scroll/keyboard handlers can clamp correctly.
    let inner_h = list_area.height.saturating_sub(2);
    app.list_view_height = inner_h;
    app.clamp_scroll();

    // Build a flat Vec<Line> from all commit items, applying a background
    // highlight to lines that belong to the currently selected commit.
    let highlight_bg = Color::DarkGray;
    let mut all_lines: Vec<Line> = Vec::new();

    // Virtual "uncommitted changes" block at the very top, when dirty.
    if app.dirty {
        // Find HEAD's lane in the graph so the marker sits on the right column.
        let head_lane = app
            .loaded
            .head_id
            .and_then(|h| app.loaded.commits.iter().position(|c| c.id == h))
            .map(|idx| app.graph_rows[idx].commit_lane)
            .unwrap_or(0);

        let lane_pad = |spans: &mut Vec<Span>| {
            for _ in 0..head_lane {
                spans.push(Span::raw("  "));
            }
        };

        // Header row.
        let mut spans: Vec<Span> = Vec::new();
        lane_pad(&mut spans);
        spans.push(Span::styled(
            "○ ",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        let label = if app.uncommitted_expanded {
            "uncommitted changes ▾"
        } else {
            "uncommitted changes ▸"
        };
        spans.push(Span::styled(
            label.to_string(),
            Style::new()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
        all_lines.push(Line::from(spans));

        // Expanded file rows.
        if app.uncommitted_expanded {
            if let Some(files) = &app.uncommitted_files {
                for fc in files {
                    let mut spans: Vec<Span> = Vec::new();
                    lane_pad(&mut spans);
                    spans.push(Span::styled(
                        "│ ",
                        Style::new().fg(Color::Yellow),
                    ));
                    let (letter, color) = match fc.status {
                        ChangeStatus::Added => ('A', Color::Green),
                        ChangeStatus::Deleted => ('D', Color::Red),
                        ChangeStatus::Modified => ('M', Color::Yellow),
                        ChangeStatus::Renamed => ('R', Color::Magenta),
                    };
                    spans.push(Span::raw("   "));
                    spans.push(Span::styled(
                        format!("{letter} "),
                        Style::new().fg(color),
                    ));
                    spans.push(Span::raw(fc.path.clone()));
                    all_lines.push(Line::from(spans));
                }
            }
        }
    }

    for (idx, c) in app.loaded.commits.iter().enumerate() {
        let row = &app.graph_rows[idx];
        let refs = app.loaded.refs_by_id.get(&c.id);
        let mut line = commit_line(row, c, refs);
        if app.selected == Some(idx) {
            apply_bg(&mut line, highlight_bg);
        }
        all_lines.push(line);

        if app.expanded[idx] {
            if let Some(files) = app.file_cache.get(&idx) {
                for fc in files {
                    let mut fline = file_line(row, fc);
                    if app.selected == Some(idx) {
                        apply_bg(&mut fline, highlight_bg);
                    }
                    all_lines.push(fline);
                }
            }
        }
    }

    let title = format!(" grove — {} ", app.repo_path.display());
    let list_para = Paragraph::new(all_lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.list_scroll, 0));
    f.render_widget(list_para, list_area);

    if app.diff_view.is_some() {
        render_diff_panel(f, right_area, app);
        render_bottom_row(f, chunks[2], app, BottomMode::Diff);
        if app.picker.is_some() {
            render_picker_overlay(f, f.area(), app);
        }
        return (toolbar_area.y, list_area, right_area);
    } else {
        app.diff_toggle_button = None;
    }

    let detail_text = match app.selected_commit() {
        Some(c) => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("commit {}", c.id_hex),
                    Style::new().yellow(),
                )),
                Line::from(format!("Author: {} <{}>", c.author, c.email)),
                Line::from(format!("Date:   {}", c.time)),
            ];
            if !c.parents.is_empty() {
                let parents_str = c
                    .parents
                    .iter()
                    .map(|p| p.to_hex_with_len(7).to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(Line::from(format!("Parents: {parents_str}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(c.summary.clone()));
            if !c.body.is_empty() {
                lines.push(Line::from(""));
                for body_line in c.body.lines() {
                    lines.push(Line::from(body_line.to_string()));
                }
            }
            lines
        }
        None => vec![Line::from("No commits")],
    };

    let detail = Paragraph::new(detail_text)
        .block(Block::default().borders(Borders::ALL).title(" details "))
        .wrap(Wrap { trim: false });
    f.render_widget(detail, right_area);

    render_bottom_row(f, chunks[2], app, BottomMode::Normal);

    if app.picker.is_some() {
        render_picker_overlay(f, f.area(), app);
    }

    (toolbar_area.y, list_area, right_area)
}

fn render_picker_overlay(f: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.picker else { return };

    // Center a small popup.
    let max_w = picker
        .items
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(20)
        .max(picker.title.chars().count())
        + 6;
    let popup_w = (max_w as u16).min(area.width.saturating_sub(4)).max(20);
    let popup_h = (picker.items.len() as u16 + 2).min(area.height.saturating_sub(4)).max(3);

    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::new();
    for (i, name) in picker.items.iter().enumerate() {
        let style = if i == picker.selected {
            Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let prefix = if i == picker.selected { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(name.clone(), style),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", picker.title));
    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, popup);
}

#[derive(Clone, Copy)]
enum BottomMode {
    Normal,
    Diff,
}

fn render_bottom_row(f: &mut Frame, area: Rect, app: &App, mode: BottomMode) {
    if let Some(input) = &app.input {
        let line = Line::from(vec![
            Span::styled(
                input.prompt.clone(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(input.buffer.clone()),
            Span::styled("█", Style::new().fg(Color::Yellow)),
            Span::raw("    "),
            Span::styled(
                "(enter to confirm, esc to cancel)",
                Style::new().add_modifier(Modifier::DIM),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let help = match mode {
        BottomMode::Normal => Paragraph::new(Line::from(vec![
            Span::styled(" j/k ", Style::new().reversed()),
            Span::raw(" move  "),
            Span::styled(" ↵ ", Style::new().reversed()),
            Span::raw(" expand  "),
            Span::styled(" b ", Style::new().reversed()),
            Span::raw(" branch  "),
            Span::styled(" c ", Style::new().reversed()),
            Span::raw(" checkout  "),
            Span::styled(" n ", Style::new().reversed()),
            Span::raw(" rename  "),
            Span::styled(" D ", Style::new().reversed()),
            Span::raw(" del  "),
            Span::styled(" t ", Style::new().reversed()),
            Span::raw(" tag  "),
            Span::styled(" q ", Style::new().reversed()),
            Span::raw(" quit "),
        ])),
        BottomMode::Diff => Paragraph::new(Line::from(vec![
            Span::styled(" j/k ", Style::new().reversed()),
            Span::raw(" scroll  "),
            Span::styled(" PgUp/PgDn ", Style::new().reversed()),
            Span::raw(" page  "),
            Span::styled(" c ", Style::new().reversed()),
            Span::raw(" changes-only  "),
            Span::styled(" esc ", Style::new().reversed()),
            Span::raw(" close  "),
            Span::styled(" q ", Style::new().reversed()),
            Span::raw(" quit "),
        ])),
    };
    f.render_widget(help, area);
}

fn render_toolbar(f: &mut Frame, area: Rect, app: &mut App) {
    // Buttons rendered as bracketed labels with reverse video.
    let labels: [(&str, ToolbarAction); 5] = [
        ("[r] refresh", ToolbarAction::Refresh),
        ("[f] fetch", ToolbarAction::Fetch),
        ("[p] pull", ToolbarAction::Pull),
        ("[t] tag", ToolbarAction::Tag),
        ("[T] push tags", ToolbarAction::PushTags),
    ];

    app.toolbar_buttons.clear();
    let mut spans: Vec<Span> = Vec::new();
    let mut col: u16 = area.x;
    for (label, action) in labels {
        // Leading space.
        spans.push(Span::raw(" "));
        col += 1;
        let padded = format!(" {label} ");
        let width = padded.chars().count() as u16;
        spans.push(Span::styled(padded, Style::new().reversed()));
        app.toolbar_buttons.push((col, col + width, action));
        col += width;
    }
    spans.push(Span::raw("    "));
    let status_style = if app.pending.is_some() {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    if app.pending.is_some() {
        spans.push(Span::styled("⟳ ", status_style));
    }
    spans.push(Span::styled(app.status.clone(), status_style));

    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

fn apply_bg(line: &mut Line, bg: Color) {
    for span in line.spans.iter_mut() {
        span.style = span.style.bg(bg);
    }
}

fn commit_line<'a>(
    row: &GraphRow,
    c: &'a CommitRow,
    refs: Option<&'a Vec<RefLabel>>,
) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    spans.extend(render_graph_spans(row, &graph::render_row(row)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{} ", &c.id_hex[..7]),
        Style::new().yellow(),
    ));

    if let Some(refs) = refs {
        for r in refs {
            let style = match r.kind {
                RefKind::Head => Style::new().fg(Color::White).bg(Color::Blue).bold(),
                RefKind::LocalBranch => Style::new().fg(Color::Black).bg(Color::Green),
                RefKind::RemoteBranch => Style::new().fg(Color::Black).bg(Color::Red),
                RefKind::Tag => Style::new().fg(Color::Black).bg(Color::Yellow),
            };
            spans.push(Span::styled(format!(" {} ", r.name), style));
            spans.push(Span::raw(" "));
        }
    }

    spans.push(Span::styled(
        format!("{:<18.18} ", c.author),
        Style::new().add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw(c.summary.clone()));
    Line::from(spans)
}

fn file_line<'a>(row: &GraphRow, fc: &'a FileChange) -> Line<'a> {
    // Render only verticals for the lanes that pass through (outgoing state),
    // so the graph keeps flowing under the file rows without redrawing the dot.
    let mut spans = render_continuation_spans(row);
    spans.push(Span::raw("    "));
    let (letter, color) = match fc.status {
        ChangeStatus::Added => ('A', Color::Green),
        ChangeStatus::Deleted => ('D', Color::Red),
        ChangeStatus::Modified => ('M', Color::Yellow),
        ChangeStatus::Renamed => ('R', Color::Magenta),
    };
    spans.push(Span::styled(format!("{letter} "), Style::new().fg(color)));
    spans.push(Span::raw(fc.path.clone()));
    Line::from(spans)
}

/// Render a commit row's cells to colored spans, with horizontal `─` filling
/// the gap between adjacent cells whenever a connector spans across.
fn render_graph_spans<'a>(row: &GraphRow, cells: &[Cell]) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    let n = cells.len();
    for i in 0..n {
        let (ch, color) = cell_to_char(cells[i]);
        spans.push(Span::styled(ch.to_string(), Style::new().fg(color)));
        if i + 1 < n {
            // Is there a horizontal connector spanning the gap between lane i and i+1?
            let cl = row.commit_lane;
            let span_check = |target: usize| -> bool {
                let lo = cl.min(target);
                let hi = cl.max(target);
                lo <= i && i < hi
            };
            let has_horizontal = row.merged_in.iter().any(|&m| span_check(m))
                || row.branched_out.iter().any(|&b| span_check(b));
            if has_horizontal {
                spans.push(Span::raw("─"));
            } else {
                spans.push(Span::raw(" "));
            }
        }
    }
    spans
}

/// File-row continuation: just verticals at every lane currently active in
/// `row.outgoing` (the lane state going downward from this commit).
fn render_continuation_spans<'a>(row: &GraphRow) -> Vec<Span<'a>> {
    let n = row
        .outgoing
        .len()
        .max(row.commit_lane + 1)
        .max(row.incoming.len());
    let mut spans: Vec<Span<'a>> = Vec::new();
    for i in 0..n {
        let active = row.outgoing.get(i).and_then(|s| s.as_ref()).is_some();
        if active {
            spans.push(Span::styled(
                "│".to_string(),
                Style::new().fg(lane_color(i)),
            ));
        } else {
            spans.push(Span::raw(" "));
        }
        if i + 1 < n {
            spans.push(Span::raw(" "));
        }
    }
    spans
}

fn cell_to_char(cell: Cell) -> (char, Color) {
    match cell {
        Cell::Empty => (' ', Color::Reset),
        Cell::Vertical(l) => ('│', lane_color(l)),
        Cell::Commit(l) => ('●', lane_color(l)),
        Cell::Horizontal => ('─', Color::Reset),
        Cell::Cross(l) => ('┼', lane_color(l)),
        Cell::CornerUL(l) => ('╯', lane_color(l)),
        Cell::CornerUR(l) => ('╰', lane_color(l)),
        Cell::CornerDL(l) => ('╮', lane_color(l)),
        Cell::CornerDR(l) => ('╭', lane_color(l)),
    }
}

fn lane_color(lane: usize) -> Color {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

fn render_diff_panel(f: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::block::Title;

    let Some(view) = &app.diff_view else {
        app.diff_toggle_button = None;
        return;
    };

    let title = format!(
        " {} {} · {} ",
        match view.status {
            ChangeStatus::Added => "[A]",
            ChangeStatus::Deleted => "[D]",
            ChangeStatus::Modified => "[M]",
            ChangeStatus::Renamed => "[R]",
        },
        view.path,
        if view.language.is_empty() {
            "?".to_string()
        } else {
            view.language.clone()
        }
    );

    // Right-aligned toggle in the top border. Clicking it flips only_changes.
    let toggle_label = if view.only_changes {
        " full file "
    } else {
        " changes only "
    };
    let toggle_text = format!("[{toggle_label}]");
    let toggle_width = toggle_text.chars().count() as u16;
    // The title sits in the top-border row, right-aligned with a 1-cell
    // padding inside the right corner. Compute the button's column range so
    // handle_mouse can dispatch clicks to it.
    let btn_row = area.y;
    let btn_end = area.x + area.width.saturating_sub(1); // just inside the ┐
    let btn_start = btn_end.saturating_sub(toggle_width);
    app.diff_toggle_button = Some((btn_row, btn_start, btn_end));

    // Inner content width (subtract block borders).
    let content_width = area.width.saturating_sub(2) as usize;
    let lines = build_diff_lines(view, content_width);

    let right_title = Title::from(Line::from(Span::styled(
        toggle_text,
        Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Right);

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title(right_title),
        )
        .scroll((view.scroll, 0));
    f.render_widget(para, area);
}

/// Produce the Vec<Line> for the diff panel, honoring the `only_changes`
/// toggle. When on, context (unchanged) lines are hidden and non-contiguous
/// hunks get separated by an ellipsis row.
fn build_diff_lines<'a>(view: &'a FileDiff, width: usize) -> Vec<Line<'a>> {
    if !view.only_changes {
        return view
            .lines
            .iter()
            .map(|dl| diff_line_to_line(dl, view, width))
            .collect();
    }

    let mut out: Vec<Line<'a>> = Vec::new();
    let mut in_hunk = false;
    let mut seen_change = false;
    for dl in &view.lines {
        match dl.kind {
            LineKind::Context => {
                // Mark end of any running hunk. The next +/- will emit a separator.
                in_hunk = false;
            }
            LineKind::Addition | LineKind::Deletion => {
                if !in_hunk && seen_change {
                    out.push(hunk_separator(width));
                }
                out.push(diff_line_to_line(dl, view, width));
                in_hunk = true;
                seen_change = true;
            }
        }
    }
    if !seen_change {
        out.push(Line::from(Span::styled(
            "(no changes)".to_string(),
            Style::new().add_modifier(Modifier::DIM),
        )));
    }
    out
}

fn hunk_separator<'a>(width: usize) -> Line<'a> {
    // "⋯" centered in the row's dim gray, matches the vertical separator of
    // conventional unified diff viewers.
    let marker = " ⋯ ⋯ ⋯ ";
    let marker_w = marker.chars().count();
    let pad = width.saturating_sub(marker_w) / 2;
    let line = format!("{}{}", " ".repeat(pad), marker);
    Line::from(Span::styled(
        line,
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
}

fn diff_line_to_line<'a>(dl: &'a DiffLine, view: &'a FileDiff, width: usize) -> Line<'a> {
    let gutter_old = dl
        .old_no
        .map(|n| format!("{n:>5}"))
        .unwrap_or_else(|| "     ".to_string());
    let gutter_new = dl
        .new_no
        .map(|n| format!("{n:>5}"))
        .unwrap_or_else(|| "     ".to_string());
    let gutter_style = Style::new().fg(Color::DarkGray);

    // Use 256-color palette indices for the diff backgrounds — they're a
    // fixed colour cube (not the 16 user-themed slots), so dark green / dark
    // red render consistently across most terminal themes.
    let (marker_text, marker_style, line_bg): (&str, Style, Option<Color>) = match dl.kind {
        LineKind::Addition => (
            " + ",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            Some(Color::Indexed(22)), // dark green
        ),
        LineKind::Deletion => (
            " - ",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            Some(Color::Indexed(52)), // dark red
        ),
        LineKind::Context => ("   ", Style::new(), None),
    };

    // Helper: every span gets the line bg if one is set, so the highlight
    // is uniform across the whole row.
    let with_bg = |style: Style| -> Style {
        if let Some(bg) = line_bg {
            style.bg(bg)
        } else {
            style
        }
    };

    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut used: usize = 0;

    let push = |spans: &mut Vec<Span<'a>>, used: &mut usize, text: String, style: Style| {
        *used += text.chars().count();
        spans.push(Span::styled(text, style));
    };

    push(&mut spans, &mut used, gutter_old, with_bg(gutter_style));
    push(&mut spans, &mut used, " ".to_string(), with_bg(Style::new()));
    push(&mut spans, &mut used, gutter_new, with_bg(gutter_style));
    push(&mut spans, &mut used, marker_text.to_string(), with_bg(marker_style));

    // Syntax-highlighted file content. New file for additions/context, old
    // file for deletions. Fall back to raw text if highlight failed.
    let hl: Option<&Vec<HlSpan>> = match (dl.kind, dl.new_no, dl.old_no) {
        (LineKind::Addition, Some(n), _) | (LineKind::Context, Some(n), _) => {
            view.hl_new.get((n as usize).saturating_sub(1))
        }
        (LineKind::Deletion, _, Some(o)) => view.hl_old.get((o as usize).saturating_sub(1)),
        _ => None,
    };

    if let Some(hl_spans) = hl {
        for s in hl_spans {
            push(&mut spans, &mut used, s.text.clone(), with_bg(Style::new().fg(s.fg)));
        }
    } else {
        push(&mut spans, &mut used, dl.text.clone(), with_bg(Style::new()));
    }

    // Pad the rest of the row with spaces carrying the bg, so the highlight
    // extends to the right edge of the panel.
    if line_bg.is_some() && used < width {
        let pad = " ".repeat(width - used);
        spans.push(Span::styled(pad, with_bg(Style::new())));
    }

    Line::from(spans)
}
