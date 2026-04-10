# grove

A terminal git graph viewer with mouse support, inline diffs, and the most common git operations baked in. Built for people who live in their terminal and don't want to leave it just to look at history.

## What it looks like

The commit graph with branch / tag chips:

```
[r] refresh  [f] fetch  [p] pull  [t] tag  [T] push tags         loaded 10000 commits
┌─ grove · ~/code/myrepo ──────────────────────────────────────────────────────────────┐
│ ●─╮  a3f9d2  main  origin/main  HEAD  Leonard  feat: add login flow                  │
│ │ │  b127cd                             Alice    fix: rate limit edge case           │
│ │ ●─╮ c8a4be  feature/auth              Bob      wip: oauth handler                  │
│ │ │ │ d5e6ff                            Carol    refactor: split util                │
│ ●─╯ │ e7f9aa                            Leonard  merge dev → main                    │
│ │   │ f1b2c3  v0.2.0                    Alice    docs: update README                 │
│ │   ●─╯ a8d4c1                          Bob      bump version                        │
│ ●─────╯ b9c2d1                          Leonard  init repo                           │
└──────────────────────────────────────────────────────────────────────────────────────┘
 j/k move   ↵ expand   b branch   c checkout   n rename   D del   t tag   q quit
```

Click a file inside an expanded commit to open a syntax-highlighted inline diff. The whole row is tinted (green for additions, red for deletions); context lines stay neutral so the diff jumps out:

```
┌─ [M] src/auth/handler.ts · TypeScript ───────────────────────────────────────────────┐
│   42    42       export async function handleLogin(req: Request) {                   │
│   43    43         const body = await req.json();                                    │
│   44       -      const user = await db.user.find(body.email);                       │
│         44 +      const user = await db.user.findUnique({                            │
│         45 +        where: { email: body.email },                                    │
│         46 +      });                                                                │
│   45    47         if (!user) return new Response('not found', { status: 404 });     │
│   46    48         return Response.json({ user });                                   │
│   47    49       }                                                                   │
└──────────────────────────────────────────────────────────────────────────────────────┘
 j/k scroll   PgUp/PgDn page   esc close diff   q quit
```

Added rows are highlighted dark green, deleted rows dark red, both spanning the full panel width. Syntax colors are mapped to the 16 ANSI slots so they pick up your terminal theme.

## Features

- **Lane-tracking graph** with proper box-drawing connectors (`╯`, `╰`, `╮`, `╭`, `┼`) and per-lane colors
- **All refs visible**, including remotes — colleagues' branches show up as red chips
- **Branch / tag / HEAD chips** next to each commit
- **Click anywhere**: click a commit to expand its file list, click a file to open the diff
- **Inline whole-file diff viewer** — see the entire file with `+`/`-` lines interleaved (like an IDE), not just unified hunks
- **Syntax highlighting for 213 languages** via [`two-face`](https://crates.io/crates/two-face) (the syntax pack `bat` uses)
- **Theme-adaptive**: highlight colors map to ANSI named slots so your terminal palette decides the actual paint
- **Mouse wheel scrolling** independent of selection — keep your highlight pinned while scrolling through history
- **Free scroll** of the commit list and the diff panel separately
- **In-tool git ops**: refresh, fetch, pull, tag, push tags, branch create / checkout / rename / delete
- **Lightweight**: a single static binary, ~10MB. Uses [`gix`](https://github.com/Byron/gitoxide) so there's no `libgit2` C dependency.

## Install

You need a Rust toolchain. If you don't have one:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source $HOME/.cargo/env
```

Then install grove from the repo (puts a `grove` binary at `~/.cargo/bin/grove`, which rustup adds to your `PATH`):

```
cargo install --git https://github.com/leonardtng/grove --locked
```

To upgrade later, run the same command again.

If you've cloned the repo locally, you can also do:

```
cargo install --path . --locked
```

## Usage

Run from anywhere inside any git repo. Two equivalent forms:

```
cd ~/code/my-project
grove
# or, as a git subcommand:
git grove
```

`grove` and `git grove` are the same binary. The git-subcommand form works because cargo also installs a `git-grove` binary, which `git` auto-dispatches when you type `git grove`.

It walks upward from `.` to find the closest `.git`, so you can be in any subdirectory. Full form:

```
grove [PATH] [--limit N | -n N]
```

- `PATH` — repo to view. Defaults to `.`. `gix` walks upward to find `.git`.
- `--limit N` / `-n N` — max commits to load. Default is 10,000.

## Keyboard

| Key | Action |
|---|---|
| `j` / `k` / arrows | Move selection one commit |
| `PgUp` / `PgDn` | Scroll 20 |
| `Home` / `End` | Jump to first/last commit |
| `Enter` / `Space` | Expand the selected commit (load files) |
| Mouse wheel | Scroll viewport (selection stays put) |
| Click commit | Select + expand |
| Click file | Open inline diff in the right pane |
| `r` | Refresh from disk |
| `f` | `git fetch --all --prune` |
| `p` | `git pull --ff-only` |
| `t` | Create tag at selected commit (prompts for name) |
| `T` | `git push origin --tags` |
| `b` | Create branch at selected commit (prompts) |
| `c` | Checkout (branch on selected commit, or detached on commit) |
| `n` | Rename current HEAD branch (prompts) |
| `D` | Delete first local branch on selected commit (force) |
| `Esc` | Close the diff view (or cancel the input prompt) |
| `q` | Quit |

In the diff view, `j`/`k`/`PgUp`/`PgDn`/`Home`/`End` and the mouse wheel scroll the diff itself. `Esc` returns to the commit list.

## Stack

- **[Rust](https://rust-lang.org)** — single static binary
- **[Ratatui](https://ratatui.rs)** — TUI rendering
- **[crossterm](https://crates.io/crates/crossterm)** — terminal backend with mouse support (SGR 1006)
- **[gix (gitoxide)](https://github.com/Byron/gitoxide)** — pure-Rust git access, no libgit2
- **[imara-diff](https://crates.io/crates/imara-diff)** — line diff (histogram algorithm)
- **[syntect](https://crates.io/crates/syntect)** + **[two-face](https://crates.io/crates/two-face)** — syntax highlighting

## Status

Early. Works on the repos I've thrown at it. Things I might add next: branch picker overlay (when multiple branches share a commit), background fetch with progress, annotated tag support, push current branch.
