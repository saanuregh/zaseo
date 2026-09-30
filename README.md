> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.


# Zaseo

Zaseo is a fork of Zed that shows [Paseo](https://paseo.sh) agents in a native
sidebar and agent tabs. Zed's built-in AI features are always off. Zaseo uses its own
`zaseo` command, `zaseo://` links, and data directory, so it can be installed
next to Zed. It does not update itself.

Run `zaseo` with no arguments. It opens projects from agents, so the command takes
`zaseo://` links but not file or folder paths, and the File menu, title bar, and welcome
page have no project-opening actions. Zed's sign-in and collaboration are removed, and
telemetry is off by default because it would go to Zed's servers.

## Upstream versions

| Project | Release |
| --- | --- |
| Zed | Between releases: `main` at [`e52ab15`](https://github.com/zed-industries/zed/commit/e52ab15), after [`v1.22.0-pre`](https://github.com/zed-industries/zed/releases/tag/v1.22.0-pre) |
| Paseo | [`v0.10.1`](https://github.com/getpaseo/paseo/releases/tag/v0.10.1), protocol v1 |

Zaseo's UI and protocol follow Paseo v0.10.1, and it was tested against a v0.10.1 daemon.
Future resyncs move both projects to stable release tags only.

## Build and run on Linux

Follow [Building Zed for Linux](./docs/src/development/linux.md), then run:

```sh
cargo run -p zed
```

If startup fails with `NoWaylandLib`, add the Wayland library directory first:

```sh
export LD_LIBRARY_PATH="$(pkg-config --variable=libdir wayland-client):$LD_LIBRARY_PATH"
```

To build a release bundle and install it as `~/.local/bin/zaseo`, run
`script/install-linux`. `script/uninstall.sh` removes the installed app but
keeps your settings and data.

### Nix

The flake builds a stable release package with the `zaseo` command:

```sh
nix build .#zaseo
```

To install it from another flake, add this repository as an input and use
`inputs.zaseo.packages.${system}.zaseo`, or apply `inputs.zaseo.overlays.default`
and use `pkgs.zaseo`. It installs next to nixpkgs' `zed-editor`.
Flakes only see files tracked by git, so new source files must be committed
before `nix build .#zaseo` includes them.

## Connect to Paseo

Zaseo connects to a Paseo daemon you install and start yourself. It does not
bundle or start Paseo. It targets Paseo v0.10.1 and shows an incompatibility
error when a daemon lacks the features it needs. At startup it connects to the
active profile; the default `Local` profile points to `ws://127.0.0.1:6767/ws`.
Use the host menu at the top of the Paseo sidebar, or **Manage Hosts…**, to
switch hosts or add one.

The Paseo sidebar (left dock, **View → Paseo Agents**) lists projects, their
workspaces, and each workspace's agents, as Paseo 0.9 does, with pinned
workspaces first. A workspace with one agent is a single row that opens the
agent. Each row's title wraps onto two lines, and under it the row shows its
project (outside the project grouping), worktree and branch, changed lines, and
last activity. A thin line under a row, and the line under an agent's tab, show
its state: yellow and moving when it needs input, blue and moving slowly while it
runs, blue when it finished unread, and red when it failed. The bell in the sidebar
header counts the agents that need you, in the most urgent one's colour, and
lists them: waiting for input first, then failed or finished and unread, each
with how long it has needed you and, for a failure, the daemon's error. A row
opens its agent, and unread ones can be marked read. The title bar shows the
same bell with how many agents are running, and after the project name the
active tab's agent and its state; it has no worktree or branch pickers, since
the sidebar shows each agent's. A failed agent's chat shows
the error above the conversation, and an agent whose provider isn't available on the
host says so and doesn't send messages. Alerts for finished or
waiting agents replace each other instead of stacking up. Rows keep their order
while the pointer is over the sidebar and re-sort when it leaves. The sidebar can also group workspaces by
status (each under its most urgent agent, as Paseo lists them), or by label. Each agent opens in its own tab with its conversation,
permission prompts, and a composer with provider, model, thinking, and mode
pickers; each opens a list with descriptions, and long lists (some hosts offer
hundreds of models) can be searched by name or description. Tool
calls show as one quiet line each ("Ran 2 commands and used 1 other tool"), and a
click opens the details in place. Once a turn finishes, its steps before the
agent's final answer (narration, tools, and thinking) fold under a "Worked for …"
line at the top; click it to show them again. A new agent starts as a draft tab; its first message creates the agent. Opening an
agent switches the window to that agent's project, opening the project if needed,
as Paseo does. Remote agents switch only when an SSH target is known (see
`editor_ssh_uri` below). The default theme is **Paseo Dark** / **Paseo Light**.

Features the connected daemon supports also appear:

- **Last turn** tab (the diff button next to the composer's microphone,
  **View → Paseo Last Turn**, or **Review** on the changed files card under the
  latest finished turn): the files the turn changed as an
  editor diff. Hunks can be edited or reverted in place. Paseo keeps no per-turn
  checkpoints, so the old text is rebuilt by undoing the turn's edits. A file
  that changed after the turn shows only its edit snippets, and files outside
  the open project are read through the daemon and shown read-only. Use Zed's
  git panel for uncommitted and branch changes.
- **Rewind** from a user message's hover row (conversation, files, or both, as
  the agent's provider allows). A rewind cannot be undone.
- **Terminals** on the Paseo host in the agent's directory, from the command
  center or `ctrl-shift-t`. Closing a tab keeps the shell running; the tab's
  **Kill** stops it.
- **Links in chat**: code spans that name a file open it (from the agent's folder,
  or the one project file a bare or partial path ends with). Code names such as
  `open_agent`, `PaseoStore` or `store::bucket` open their definition when clicked,
  found by the project's language servers like Zed's Go to Symbol in Project:
  several matches open that picker with the name typed, none searches the project.
  `ctrl`-click (`cmd` on macOS) opens Zed's Text Finder with the name instead, to
  see where it is used.
- **Subagents** (Claude Tasks and other provider subagents): a track above the
  composer counts them (`3 subagents · 1 working`) and always lists running
  ones on one line each: title, the provider's summary (for Claude: type,
  model, effort and tokens) and how long it has run. Click it to list every
  subagent; a row opens the subagent's conversation in a read-only tab, and
  finished ones can be archived from the track. A subagent's tool row shows its
  action count (and its latest action while it runs), opens the subagent's tab
  from its ↗ button, and expands to the numbered action list.
- **Checkout footer** under the composer: where the agent works (**Local
  checkout** or its worktree) and its branch. In a new-agent draft it picks
  **Local checkout** or **New worktree**, which starts the agent on a new branch
  in its own git worktree, and the base branch that worktree branches off; the
  base defaults to the current branch's upstream, as in Paseo.
- **Dictation** with the composer's microphone button, when the daemon has
  speech-to-text enabled.
- **Provider Usage** (**View → Paseo Provider Usage**, or the host menu): each
  provider's plan limits.
- **Send code to an agent** from the editor: **Add Selection to Agent** (right
  click, or `ctrl->`) adds `@file:lines` and the selected code, or the cursor's
  line; the project panel's **Add to Agent** adds `@path` mentions; **Ask Agent
  to Fix** in the code actions menu (`ctrl-.`) on an error or warning adds the
  problem and its lines. The text goes to the agent tab used last in this
  project, else the agent last focused anywhere (opened here), else a new agent
  draft, and is never sent on its own.
- **Workspaces and projects**:
  - A workspace row's right-click menu has rename, **Mark as Read** or **Mark as
    Unread**, pin, labels (assign, create, rename, recolor, delete), scripts
    (start, stop, open, run a blocked setup), copy path or branch, and **Archive
    Workspace…**. Archiving a Paseo worktree workspace removes the worktree folder
    once no other workspace uses it; the branch is kept.
  - A project's menu has a new agent, **New Workspace…** (Local or a new worktree
    from a base branch, starting a chat or only a terminal), **Paseo Worktrees…**
    (list and archive the project's Paseo worktrees), rename, icon, and remove.
    Removing a project never changes files on disk.
  - The sidebar's grouping menu adds a project or creates a project directory.
- **History** (the sidebar row under **New workspace**, or **Open history** in the
  command palette), as in Paseo: every agent the host keeps, active and archived,
  newest first under Today, Yesterday, This week, This month and Older. A row
  shows its workspace › provider and title, an **Archived** or pending badge, the
  project, branch and last activity. The host searches workspace names, titles,
  branches and projects; **Load more** fetches the next page. A row opens the
  agent in its workspace; an archived agent opens with **This agent is archived**
  and **Unarchive** in place of the composer. Right-click adds **Unarchive**,
  **Restore Workspace** (when its workspace can come back) and **Delete
  Permanently**, or **Archive** for an active agent. History reloads each time it
  is opened, not live.
- **Command palette**: Zed's palette is the only one, on `ctrl-shift-p` and the
  keys above. Typing also finds agents (**Agent: title — project · status**), the
  current agent's terminals, and Paseo's commands under their own names. Zed's
  collaboration, account, update, feedback, threads and assistant commands are
  hidden.
- **Daemon Status** (host menu): version, PID, listen address, relay, and each
  provider's availability; refresh providers, run a provider's diagnostic, and
  restart or update the daemon. Paseo Desktop's own daemon updates only through
  Paseo Desktop.
- **Agent edits in editors**: unreviewed edits from every agent in the project
  show inline in open editors until kept or rejected, like Zed's own agent. Each
  hunk has **Keep** and **Reject** (Reject restores the text before it), and the
  toolbar has **Keep All**, **Reject All**, and hunk navigation. Your own typing
  isn't highlighted, an agent edit you undo by hand is dropped, and kept or
  rejected edits stay reviewed after a restart. Whole-file writes aren't
  highlighted, since the text they replaced is unknown, and neither is a pure
  deletion the daemon can't place. While a file shows agent edits, its git hunks
  are hidden. Only local projects with agents on this machine are covered.

Keyboard shortcuts (`ctrl` on Linux and Windows, `cmd` on macOS):

| Keys | Action |
| --- | --- |
| `ctrl-alt-p` | Focus or hide the Paseo sidebar |
| `ctrl-alt-n`, or `ctrl-n` in Paseo views | New workspace |
| `ctrl-alt-t`, or `ctrl-t` in Paseo views | New agent in the shown workspace |
| `ctrl-alt-k`, or `ctrl-k` in Paseo views | Command palette, with agents |
| `ctrl-alt-]` / `ctrl-alt-[` | Next / previous agent |
| `ctrl-1` … `ctrl-9` in Paseo views | Open the agent at that sidebar position |
| `enter` / `shift-enter` | Send (steers a running agent) / new line |
| `ctrl-enter` | Queue the message until the agent finishes |
| `escape` | Interrupt the running agent |
| `shift-tab` | Cycle the permission mode |
| `ctrl-/` | Choose the model |
| `ctrl-alt-/` (Linux and Windows) | Choose the permission mode |
| `ctrl-l` | Focus the composer |
| `shift-alt-a` / `shift-alt-x` | Accept / deny the pending permission |
| `ctrl-shift-backspace`, or `ctrl-backspace` in the sidebar | Archive the agent |
| `ctrl-shift-down` | Scroll to the latest message |
| `ctrl-f` in the sidebar (Linux and Windows) | Filter agents |
| `ctrl-=` / `ctrl--` / `ctrl-0` | Zoom the chat and editors in / out / reset |
| `f2` | Rename the agent |
| `ctrl-e` in Paseo views | Open the Last turn tab |
| `ctrl-shift-t` in Paseo views | New terminal in the agent's directory |
| `ctrl-d` in the composer | Start or stop dictation |
| `ctrl->` in the editor (`ctrl-shift-.` on Windows) | Add the selection to the agent |
| `alt-y` / `ctrl-alt-z` on an agent edit (`cmd-y` / `cmd-alt-z` on macOS) | Keep / reject it |
| `shift-alt-y` / `shift-alt-z` on a file with agent edits | Keep / reject all of them |

In the composer, `/` lists the agent's commands and `@` completes file paths.
Pasted images are sent with the message. The paperclip button attaches files:
PNG, JPEG, GIF and WebP files attach as images, and other files upload to the
Paseo host, where the agent reads them.

Profiles are stored under `paseo.profiles` in your settings:

- `target_uri`: a `ws://` or `wss://` daemon URL, or
  `ssh://user@host:port?daemonPort=6767` to reach a remote daemon's loopback
  port through `ssh -W`.
- `editor_ssh_uri` (direct URLs only): the SSH host that **Open Workspace** uses
  to open an agent's directory remotely. A direct URL to another machine needs
  it, because every agent opens in an editor workspace for its folder; a
  `localhost` URL doesn't. SSH profiles use their own target for this.

SSH editing (including project switching for remote agents) needs a remote
server on the host. Builds run with `cargo run` compile and upload one. Installed
release builds do not download Zed's server, so SSH projects fail unless a
matching server is already in the host's `~/.zed_server`.

Zaseo keeps passwords in memory only and never saves them. It rejects URLs that
contain credentials. When no password was typed, a `ws://` or `wss://` profile that
points at the daemon running on this machine signs in with that daemon's local
credential (`$PASEO_HOME/local-credential`, default `~/.paseo`), as the Paseo CLI
does, so a password-protected local daemon needs no password. SSH profiles always
use a typed password. A remote `ws://` connection is only as private as the
network or VPN it runs over.

## Settings

The settings window's **Paseo** page, or the `paseo` key in `settings.json`,
controls how chats, the sidebar and alerts behave. The defaults and their meaning
are in `assets/settings/default.json` under `paseo`:

- `chat`: prose `font_family`, `font_size` (unset follows the UI font size; editor
  zoom zooms it), `line_height`, `line_length` (characters), whether
  finished turns start folded (`fold_finished_turns`), and whether thinking shows
  while an agent works (`show_thinking`).
- `sidebar`: `grouping` (the grouping menu sets it too), `title_lines`, and whether
  sidebar rows and tabs animate: the activity line, and a row easing in or
  flashing when its agent appears or changes state (`animate_status`).
- `alerts`: whether agents raise `toasts`, whether the bell shows its count
  (`bell_count`), and whether agents raise `system_notifications` while no Zaseo
  window has focus (clicking one opens the agent).

Zed's own agent is off in Zaseo, so its panel, inline assistant and settings pages
are gone; edit predictions keep their own settings page.

## Terms

Zaseo uses Paseo's words for Paseo things, and Zed's own panels say "folder" where
Zed says "project".

| Word | In the Paseo sidebar and chats | In Zed's own panels |
| --- | --- | --- |
| Project | A folder or repository added to Paseo. Sidebar rows group by it. | Not used. Zed's set of open folders is called "folders", and Paseo text calls it "the editor". |
| Workspace | A set of agent tabs working in one folder of a project: its root or a Paseo worktree. Several workspaces can share a folder. **Open in Editor** opens its folder. | Not shown by that name. |
| Worktree | A git worktree Paseo made for a workspace, under `~/.paseo/worktrees/`. | One root folder of the editor's open folders. |
| Agent | One chat with one provider, in one workspace. It opens as a tab. | — |
| Subagent | An agent another agent started. It opens as a read-only tab. | — |

A workspace with one agent shows as one sidebar row with the workspace's name, and
that agent's tab uses the same name. Renaming either renames the workspace. The
agent's menu (rename, copy ID, fork, reload, archive) is the ⋯ button at the right
of the tab bar.

Each set of open folders shows the agent tabs of one Paseo workspace at a time,
with a tab for each of its agents except those another agent started; a tab you
close stays closed until you open that agent again or restart Zaseo. Clicking an
agent or a workspace in the sidebar switches to its folder, opening it if needed,
and shows its workspace's tabs; the tabs of other workspaces on the same folder
are hidden, keeping unsent text while Zaseo runs, until their workspace is shown
again. Open files, splits, and terminals belong to the folder,
so every workspace on it shares them. When another window already shows the
folder, that window comes forward. If the folder can't be opened, Zaseo shows why
and stays where you are.

**New workspace** (the sidebar's top row, or **New Paseo Workspace** in the
menu) starts a new workspace with a new agent, as do a project's new-agent
entries. **New agent** adds an agent to the workspace being shown, like Paseo's
new tab. Archiving a
workspace closes its tabs, and closes its folder too when it was a Paseo worktree.
To see two folders side by side, give each its own window. Chats opened while no
folder is open go to their own folder when you switch away, and drafts without an
agent yet go to the folder you switch to.

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
