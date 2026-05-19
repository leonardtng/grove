use anyhow::{Context, Result};
use gix::ObjectId;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

pub struct CommitRow {
    pub id: ObjectId,
    pub id_hex: String,
    pub parents: Vec<ObjectId>,
    pub author: String,
    pub email: String,
    pub time: String,
    pub summary: String,
    pub body: String,
}

pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Head,
}

pub struct LoadedRepo {
    pub repo: gix::Repository,
    pub commits: Vec<CommitRow>,
    pub refs_by_id: HashMap<ObjectId, Vec<RefLabel>>,
    pub head_id: Option<ObjectId>,
    /// Canonical "main" branch chosen by upstream-name priority (main →
    /// master → origin/main → origin/master). `None` if none exist.
    pub upstream_ref: Option<String>,
    /// Set of branch ref names (as displayed, e.g. "feature/foo",
    /// "origin/feature/foo") whose commits are either ancestor-reachable
    /// from `upstream_ref` (direct/ff merge) or patch-equivalent to commits
    /// in it (squash/rebase merge).
    pub merged_branches: HashSet<String>,
}

pub fn load_repo(repo_path: &Path, limit: usize) -> Result<LoadedRepo> {
    let repo = gix::discover(repo_path)
        .with_context(|| format!("opening git repo at {}", repo_path.display()))?;

    let head_id = repo.head()?.try_into_peeled_id()?.map(|id| id.detach());

    let refs_by_id = load_refs(&repo)?;

    // Walk from EVERY ref tip (HEAD + local branches + remote tracking branches
    // + tags) so colleagues' branches show up too. Filter to commits — tags
    // can point at blobs/trees, which the rev walker rejects.
    let mut tips: Vec<ObjectId> = refs_by_id
        .keys()
        .copied()
        .filter(|id| is_commit(&repo, *id))
        .collect();
    if let Some(h) = head_id {
        if !tips.contains(&h) && is_commit(&repo, h) {
            tips.push(h);
        }
    }

    let commits = load_commits(&repo, &tips, limit)?;

    let work_dir = repo
        .work_dir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo_path.to_path_buf());
    let (upstream_ref, merged_branches) = compute_merged_branches(&work_dir, &refs_by_id);

    Ok(LoadedRepo {
        repo,
        commits,
        refs_by_id,
        head_id,
        upstream_ref,
        merged_branches,
    })
}

/// Detect the canonical upstream branch and which other branches have been
/// merged into it (either by ancestor reachability or by patch equivalence —
/// i.e. squash/rebase merges).
fn compute_merged_branches(
    work_dir: &Path,
    refs_by_id: &HashMap<ObjectId, Vec<RefLabel>>,
) -> (Option<String>, HashSet<String>) {
    // Collect all branch names that actually exist (local + remote).
    let mut all_branches: HashSet<String> = HashSet::new();
    for labels in refs_by_id.values() {
        for l in labels {
            if matches!(l.kind, RefKind::LocalBranch | RefKind::RemoteBranch) {
                all_branches.insert(l.name.clone());
            }
        }
    }

    let candidates = ["main", "master", "origin/main", "origin/master"];
    let upstream: Option<String> = candidates
        .iter()
        .find(|c| all_branches.contains(**c))
        .map(|c| c.to_string());

    let Some(up) = upstream.clone() else {
        return (None, HashSet::new());
    };

    // Don't dim the upstream itself or its local/remote sibling.
    let siblings: HashSet<String> = {
        let mut s = HashSet::new();
        s.insert(up.clone());
        if let Some(rest) = up.strip_prefix("origin/") {
            s.insert(rest.to_string());
        } else {
            s.insert(format!("origin/{up}"));
        }
        s
    };

    // Tier 1: branches whose tips are ancestors of upstream (direct/ff merges).
    // One git call instead of N. The `--merged` flag also returns the upstream
    // itself, so we filter siblings out below.
    let mut merged: HashSet<String> = HashSet::new();
    if let Ok(o) = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--merged",
            &up,
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ])
        .current_dir(work_dir)
        .output()
    {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let name = line.trim();
                if name.is_empty() || siblings.contains(name) {
                    continue;
                }
                if all_branches.contains(name) {
                    merged.insert(name.to_string());
                }
            }
        }
    }

    // Tier 2: for branches NOT directly merged, check patch-equivalence
    // (squash / rebase). `git cherry <upstream> <branch>` returns one line
    // per commit in branch-not-upstream: `- <sha>` if patch-equivalent in
    // upstream, `+ <sha>` if not. Branch is fully patch-merged iff every
    // line begins with `-` (or output is empty).
    for branch in &all_branches {
        if siblings.contains(branch) || merged.contains(branch) {
            continue;
        }
        if is_patch_merged(work_dir, &up, branch) {
            merged.insert(branch.clone());
        }
    }

    (upstream, merged)
}

fn is_patch_merged(work_dir: &Path, upstream: &str, branch: &str) -> bool {
    let out = std::process::Command::new("git")
        .args(["cherry", upstream, branch])
        .current_dir(work_dir)
        .output();
    let Ok(o) = out else { return false };
    if !o.status.success() {
        return false;
    }
    let s = String::from_utf8_lossy(&o.stdout);
    if s.trim().is_empty() {
        return true;
    }
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.starts_with("- "))
}


fn is_commit(repo: &gix::Repository, id: ObjectId) -> bool {
    matches!(
        repo.find_object(id).map(|o| o.kind),
        Ok(gix::object::Kind::Commit)
    )
}

fn load_commits(
    repo: &gix::Repository,
    tips: &[ObjectId],
    limit: usize,
) -> Result<Vec<CommitRow>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }

    let walk = repo
        .rev_walk(tips.iter().copied())
        .sorting(gix::traverse::commit::simple::Sorting::ByCommitTimeNewestFirst)
        .all()?;

    let mut out = Vec::with_capacity(limit);
    for info in walk.take(limit) {
        let info = info?;
        let commit = info.object()?;
        let msg = commit.message()?;
        let summary = msg.summary().to_string();
        let body = msg.body.map(|b| b.to_string()).unwrap_or_default();
        let author = commit.author()?;
        let time = author.time;
        let time_str = format_time(time.seconds, time.offset);
        let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();

        out.push(CommitRow {
            id: info.id,
            id_hex: info.id.to_hex().to_string(),
            parents,
            author: author.name.to_string(),
            email: author.email.to_string(),
            time: time_str,
            summary,
            body,
        });
    }
    Ok(out)
}

fn load_refs(repo: &gix::Repository) -> Result<HashMap<ObjectId, Vec<RefLabel>>> {
    let mut by_id: HashMap<ObjectId, Vec<RefLabel>> = HashMap::new();

    // HEAD: mark whatever it points to.
    if let Ok(mut head) = repo.head() {
        if let Ok(Some(id)) = head.try_peel_to_id_in_place() {
            by_id.entry(id.detach()).or_default().push(RefLabel {
                name: "HEAD".to_string(),
                kind: RefKind::Head,
            });
        }
    }

    let platform = repo.references()?;
    for r in platform.all()?.flatten() {
        let full_name = r.name().as_bstr().to_string();
        let (display, kind) = classify_ref(&full_name);
        // Peel to commit id.
        let mut r = r;
        let id = match r.peel_to_id_in_place() {
            Ok(id) => id.detach(),
            Err(_) => continue,
        };
        by_id
            .entry(id)
            .or_default()
            .push(RefLabel { name: display, kind });
    }

    Ok(by_id)
}

fn classify_ref(full: &str) -> (String, RefKind) {
    if let Some(rest) = full.strip_prefix("refs/heads/") {
        (rest.to_string(), RefKind::LocalBranch)
    } else if let Some(rest) = full.strip_prefix("refs/remotes/") {
        (rest.to_string(), RefKind::RemoteBranch)
    } else if let Some(rest) = full.strip_prefix("refs/tags/") {
        (rest.to_string(), RefKind::Tag)
    } else {
        (full.to_string(), RefKind::LocalBranch)
    }
}

fn format_time(seconds: i64, offset: i32) -> String {
    // Naive YYYY-MM-DD HH:MM rendering in the commit's local zone.
    let total = seconds + offset as i64;
    let (y, mo, d, h, mi) = epoch_to_ymdhm(total);
    let sign = if offset >= 0 { '+' } else { '-' };
    let oh = offset.abs() / 3600;
    let om = (offset.abs() % 3600) / 60;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} {sign}{oh:02}{om:02}")
}

fn epoch_to_ymdhm(secs: i64) -> (i32, u32, u32, u32, u32) {
    // Days since 1970-01-01 (proleptic Gregorian).
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;

    // Civil from days algorithm (Howard Hinnant).
    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi)
}
