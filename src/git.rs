use anyhow::{Context, Result};
use gix::ObjectId;
use std::{collections::HashMap, path::Path};

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

    Ok(LoadedRepo {
        repo,
        commits,
        refs_by_id,
        head_id,
    })
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
