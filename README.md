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
agent. Rows keep their order while the pointer is over the sidebar and re-sort
when it leaves. The sidebar can also group agents by status, or workspaces by
label. Each agent opens in its own tab with its conversation,
permission prompts, and a composer with model, thinking, and mode pickers. A new
agent starts as a draft tab; its first message creates the agent. Opening an
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
- **Subagents** (Claude Tasks and other provider subagents): a track above the
  composer counts them (`3 subagents · 1 working`) and always lists running
  ones on one line each with their latest action. Click it to list every
  subagent; a row opens the subagent's conversation in a read-only tab, and
  finished ones can be archived from the track. A subagent's tool row shows its
  action count (and its latest action while it runs), opens the subagent's tab
  from its ↗ button, and expands to the numbered action list.
- **New worktree** isolation in a new-agent draft starts the agent on a new
  branch in its own git worktree. **Base** picks the branch it branches off;
  it defaults to the current branch's upstream, as in Paseo.
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
  - Archived agents group under their workspace when it can be restored, with
    **Restore**.
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
| `ctrl-alt-n`, or `ctrl-n` in Paseo views | New agent |
| `ctrl-alt-k`, or `ctrl-k` in Paseo views | Command center |
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
Pasted images are sent with the message.

Profiles are stored under `paseo.profiles` in your settings:

- `target_uri`: a `ws://` or `wss://` daemon URL, or
  `ssh://user@host:port?daemonPort=6767` to reach a remote daemon's loopback
  port through `ssh -W`.
- `editor_ssh_uri` (optional, direct URLs only): the SSH host that **Open
  Workspace** uses to open an agent's directory remotely. SSH profiles use their
  own target for this.

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
