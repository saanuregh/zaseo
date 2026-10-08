# Zaseo guide

This guide covers how Zaseo differs from Zed, its Paseo features, building from source,
and upstream versions. For downloads and first run, see the [README](../README.md).

## How Zaseo differs from Zed

### Command line and links

- `zaseo` takes `zaseo://` links only. File and folder paths, `-` (stdin), `--diff`,
  `file://` and `ssh://` arguments are rejected, because projects open from agents.
  `--uninstall` and `--dev-container` are gone.
- `zaseo://` replaces `zed://`, including the `/settings`, `/extension`, `/ssh` and
  `/git` routes. `zaseo://` links in the terminal are clickable.
- Zaseo registers no file types, so it doesn't appear in "Open With".
- On macOS, **Install CLI** links `/usr/local/bin/zaseo` and refuses to replace
  anything already there.

### Files and identity

Zaseo installs next to Zed and shares nothing with it:

| | Linux | macOS |
| --- | --- | --- |
| Settings | `~/.config/zaseo` | `~/.config/zaseo` |
| Data | `~/.local/share/zaseo` | `~/Library/Application Support/Zaseo` |

The app ID is `local.zaseo.Zaseo`, and the app reports Zaseo's own version, not Zed's.

### Removed or changed

- **Zed's AI.** `disable_ai` defaults to `true`, and Zed's agent panel, inline assistant,
  threads, and their settings pages are gone. Edit predictions keep their own settings
  page.
- **Accounts and collaboration.** Sign-in, calls, the collab panel and their settings
  are removed.
- **Updates.** `auto_update` is off and the updater never runs. Update Zaseo by
  installing a newer build.
- **Telemetry.** `telemetry.diagnostics` and `telemetry.metrics` default to `false`, and
  the Privacy settings and onboarding toggles are gone, because the data would go to
  Zed's servers.
- **Opening folders by hand.** The File menu, welcome page, title bar and command
  palette have no Open, Open Recent, Open Remote or Add Folder actions, and their keys
  (`ctrl-o`, `ctrl-r`, `ctrl-shift-a` and the rest) are unbound. The title bar's project
  name is a plain label.
- **Dev containers** are removed.
- **Welcome and onboarding.** The welcome page offers **New Workspace**, **New Agent**
  and **Recent Folders**. Onboarding replaces settings import and agent setup with
  **Paseo Hosts**.
- **Menus.** **Close Project** is **Close Folders**. View adds **Toggle Editor**,
  **Paseo Agents**, **Paseo Chats**, **New Paseo Agent**, **New Paseo Workspace**,
  **Paseo History**, **Paseo Last Turn** and **Paseo Usage**. Help links to the Zaseo repository and to Zed and Paseo documentation.
- **Agent entry points in the editor.** The pane's `+` menu has **New Agent**, and the
  editor toolbar has **Add Selection to Agent** in place of Inline Assist.

## Connections

Zaseo connects to Paseo daemons you install and start yourself. It does not bundle or
start Paseo. It shows an incompatibility error when a daemon lacks a feature it needs.

At startup Zaseo connects to every host at once and lists all their agents in one
sidebar. The host button at the top of the sidebar shows a dot for the worst host
status. Its menu:

- adds hosts and picks the default host for new agents (**Manage Hosts…**),
- reconnects one host (click it) or all of them (**Reconnect All**),
- opens [Usage](#usage) and [Daemon Status](#daemon-status).

With two or more hosts, the sidebar shows a row for each host that is offline, rejects
its password, or is a duplicate of another, with **Try again**.

### Profiles

Hosts are stored under `paseo.profiles` in your settings, and `paseo.active_profile`
names the default host for new agents.

- `name`: the host's name in the sidebar and menus.
- `target_uri`: a `ws://` or `wss://` URL ending in `/ws`, or
  `ssh://user@host:port?daemonPort=6767` to reach a remote daemon's loopback port
  through `ssh -W`.
- `editor_ssh_uri` (`ws://` and `wss://` hosts only): the SSH host Zaseo uses to open an
  agent's folder. A URL to another machine needs it, because every agent opens in an
  editor for its folder; a `localhost` URL doesn't. SSH hosts use their own target.
- `client_id`: leave it empty to use Zaseo's generated ID.

### SSH editing

Opening a remote agent's folder needs a Zed remote server on the host. Builds run with
`cargo run` compile and upload one. Release builds install Zed's official server for
the release pinned in `UPSTREAM_REMOTE_SERVER_TAG` (`crates/remote/src/transport.rs`),
whose protocol matches Zaseo's. The host downloads it into `~/.zed_server`. If that
fails, or the host's `ssh_connections` entry sets `upload_binary_over_ssh`, Zaseo
downloads it on this machine and uploads it.

### Passwords and credentials

- Zaseo keeps passwords in memory only and never saves them.
- It rejects URLs that contain credentials.
- When no password was typed, a `ws://` or `wss://` host that points at the daemon on
  this machine signs in with that daemon's local credential
  (`$PASEO_HOME/local-credential`, default `~/.paseo`), as the Paseo CLI does. A
  password-protected local daemon therefore needs no password.
- SSH hosts always use a typed password.
- A remote `ws://` connection is only as private as the network or VPN it runs over.

## Window layout

From left to right a window shows four areas, and each one hides on its own:

| Area | Holds | Show or hide |
| --- | --- | --- |
| Agents list | Every host's agents | `ctrl-alt-1` or `ctrl-alt-p` |
| Chat panel | Agent chats, in tabs and splits | `ctrl-alt-2` |
| Editor | Files and every other tab | `ctrl-alt-3` |
| Right dock | Folder, outline, git and the other panels | `ctrl-alt-4` or `ctrl-alt-b` |

The chat panel starts in the left dock, so `ctrl-b` toggles it too. It can move to the
right dock instead.

The layout buttons in the status bar do the same, each at the edge on the side of its
area. Each is highlighted while its area shows, and the agents button shows a dot while
the list is hidden and an agent needs you. With the editor hidden the chat panel takes
its width, and the editor stays hidden for that workspace after a restart. Opening an
agent shows the chat panel, and opening a file shows the editor.

Chats open only in the chat panel, and files never do. Dragging a chat into the editor,
or a file into the chat panel, does nothing.

- The tab bar's **+** starts a new agent. An empty panel says "No agent chats open" with
  a **New Agent** button.
- The split button and **Split Right** and the other split commands start a new agent
  beside the current one, since a chat can't show twice. The split-and-move commands
  move the current chat into the new split instead.
- Chat tabs drag between splits, and a split can be zoomed.
- The panel restores its chats and splits for each workspace after a restart. Drafts
  and subagent tabs are not restored.

## Sidebar

The agents list (**View → Paseo Agents**) runs down the window's left edge and lists
projects, their workspaces, and each workspace's agents, with pinned workspaces first.
One list serves every project open in the window.

- A workspace with one agent is a single row that opens the agent.
- A row's title takes one line by default (`paseo.sidebar.title_lines`, up to 4). Under
  it the row shows its host (when several are listed), its project (outside project
  grouping), worktree and branch, and last activity.
- Rows keep their order while the pointer is over the sidebar and re-sort when it
  leaves.
- The filter field (`ctrl-f` in the sidebar) matches titles, projects, providers, and
  the start of agent IDs.
- The grouping menu groups workspaces by project, by status (each under its most urgent
  agent, as Paseo lists them), or by label. With two or more hosts it can also hide
  hosts. It adds a project or creates a project directory.
- The header has a command palette button.

### Row menus

Right-click a row for its menu.

- **Agent:** Open, **Fork**, **New Agent in Project**, **Open in Editor**, **Mark as
  Read**, **Copy Agent ID**, **Copy Path**, **Archive**.
- **Workspace:** **New Agent Here**, rename, **Mark as Read** or **Mark as Unread**, pin,
  labels (assign, create, rename, recolor, delete), scripts (start, stop, open, run a
  blocked setup), copy path or branch, reveal in the file manager, and **Archive
  Workspace…**. Archiving a Paseo worktree workspace removes the worktree folder once no
  other workspace uses it; the branch is kept.
- **Project:** a new agent, **New Workspace…** (Local or a new worktree from a base
  branch, starting a chat or only a terminal), **Paseo Worktrees…** (list and archive
  the project's Paseo worktrees), rename, icon or **Use Automatic Icon**, **Copy Path**,
  reveal in the file manager, and remove. Removing a project never changes files on
  disk.

### Agent state

A thin line under a sidebar row, and the line under an agent's tab, show the agent's
state:

| Line | State |
| --- | --- |
| Yellow, moving | Needs input |
| Blue, moving slowly | Running |
| Blue | Finished, unread |
| Red | Failed |

The bell in the sidebar header counts the agents that need you, in the most urgent
one's colour. It lists them: waiting for input first, then failed or finished and
unread. Each entry shows how long the agent has needed you and, for a failure, the
daemon's error. An entry opens its agent, and unread ones can be marked read.

Toasts for finished, failed or waiting agents replace each other instead of stacking
up.

### Title bar

The title bar shows the same bell with how many agents are running. After the folder
name it shows the agent of the chat in front and its state. Zed's branch and worktree
buttons are hidden by default (`title_bar.show_branch_name` and
`title_bar.show_worktree_name`), since the sidebar shows each agent's.

## Agent tabs

Each agent opens in its own tab in the chat panel with its conversation, permission
prompts, and a composer.

- A new agent starts as a draft tab, with a host picker (when there are several hosts)
  and a folder picker. Its first message creates the agent.
- A failed agent's chat shows the error above the conversation.
- An agent whose provider isn't available on the host says so and doesn't send
  messages.
- Opening an agent switches the window to that agent's folder, opening it if needed, as
  Paseo does. Remote agents switch only when an SSH target is known (see
  `editor_ssh_uri` under [Profiles](#profiles)).
- The ⋯ button at the right of the tab bar has rename, copy ID, **Fork Agent**, reload,
  and archive.

### Composer

- Provider, model, thinking, and mode pickers. Each opens a list with descriptions.
  Long lists (some hosts offer hundreds of models) can be searched by name or
  description.
- After the pickers come the provider's own features, as the daemon defines them, such
  as Codex's **Speed** menu, Claude's **Fast** toggle, and **Plan**. A speed option turns
  yellow while a non-default choice is picked. A draft's choices apply when its agent
  is created.
- The context ring shows how full the agent's context window is. Hover it for token
  counts, session cost, and the usage of the account the agent runs under.
- A message sent while the agent works steers its turn. Set
  `paseo.chat.send_behavior` to `"interrupt"` to stop the turn and start a new one
  instead. A queued message sent automatically when the turn ends always steers, so a
  turn the agent starts by itself at that moment is not cut short.
- Queued messages can be sent now, or edited back into the composer.
- When a provider accepts a change with a note, such as a mode change that applies after
  the current turn, the note shows as a toast.
- `/` lists the agent's commands and skills, and `@` completes file paths.
- Pasted images are sent with the message.
- The paperclip button attaches files up to 50 MB. PNG, JPEG, GIF and WebP files attach
  as images; other files upload to the Paseo host, where the agent reads them.

### Conversation

- Each run of tool calls shows as one quiet line ("Ran 2 commands and used 1 other
  tool"). A click opens the details in place.
- Once a turn finishes, its steps before the agent's final answer (narration, tools,
  and thinking) fold under a "Worked for …" line at the top. Click it to show them
  again.

## Features that depend on the daemon

These appear when the connected daemon supports them.

### Last turn

Open it with the diff button next to the composer's microphone, **View → Paseo Last
Turn**, or **Review** on the changed files card under the latest finished turn.

- Shows the files the turn changed as an editor diff. Hunks can be edited or reverted
  in place.
- Paseo keeps no per-turn checkpoints, so the old text is rebuilt by undoing the
  turn's edits.
- A file that changed after the turn shows only its edit snippets.
- Files outside the open folder are read through the daemon and shown read-only.
- Use Zed's git panel for uncommitted and branch changes.

### Rewind

Rewind from a user message's hover row: the conversation, the files, or both, as the
agent's provider allows. Rewinding the conversation puts that message back in an empty
composer. A rewind cannot be undone.

### Terminals

Terminals run on the Paseo host in the agent's directory. Open one with **New
terminal** in the command palette or `ctrl-shift-t`. Closing a tab keeps the shell
running; the tab's **Kill** stops it.

### Links in chat

- Code spans that name a file open it, from the agent's folder or the one project file
  a bare or partial path ends with.
- Code names such as `open_agent`, `PaseoStore` or `store::bucket` open their
  definition when clicked. They are found by the project's language servers, like
  Zed's Go to Symbol in Project. Several matches open that picker with the name typed;
  none searches the project.
- `ctrl`-click (`cmd` on macOS) opens Zed's Text Finder with the name instead, to see
  where it is used.

### Subagents

Subagents are Claude Tasks and other provider subagents.

- A track above the composer counts them (`3 subagents · 1 working`) and always lists
  running ones on one line each: title, the provider's summary (for Claude: type,
  model, effort and tokens) and how long it has run.
- Click the track to list every subagent. A row opens the subagent's conversation in a
  read-only tab, and finished ones can be archived from the track.
- A subagent's tool row shows its action count (and its latest action while it runs),
  opens the subagent's tab from its ↗ button, and expands to the numbered action list.

### Checkout footer

The footer under the composer shows where the agent works (**Local checkout** or its
worktree) and its branch. In a new-agent draft it picks **Local checkout** or **New
worktree**. A new worktree starts the agent on a new branch in its own git worktree,
off a base branch you choose. The base defaults to the current branch's upstream, as
in Paseo.

### Dictation

Use the composer's microphone button when the daemon has speech-to-text enabled.

### Usage

**View → Paseo Usage**, the host menu's **Usage**, or a click on the status-bar usage
chip opens Usage. It shows one card per account: each limit window with its reset time,
balances, and why an account can't be read, such as an expired login and the command
that refreshes it. Refresh one card or all of them. Pick **Used** or **Remaining** for
the percentages. With several hosts connected, pick which host to show.

The status-bar chip shows the account the focused agent runs under. Older daemons,
which report usage per provider, show that provider's usage instead.

### Send code to an agent

- **Add Selection to Agent** (right-click, the editor toolbar, or `ctrl->`) adds
  `@file:lines` and the selected code, or the cursor's line.
- The project panel's **Add to Agent** adds `@path` mentions.
- **Ask Agent to Fix** in the code actions menu (`ctrl-.`) on an error or warning adds
  the problem and its lines.

The text goes to the agent tab used last in this project, else the agent last focused
anywhere (opened here), else a new agent draft. From a subagent's tab it goes to the
parent agent. It is never sent on its own.

### History

Open it from the sidebar row under **New workspace**, **View → Paseo History**, or
**Open history** in the command palette. As in Paseo, it lists every agent the host
keeps, active and archived, newest first under Today, Yesterday, This week, This month
and Older.

- A row shows its workspace › provider and title, an **Archived** or pending badge,
  the project, branch and last activity.
- The host searches workspace names, titles, branches and projects. **Load more**
  fetches the next page.
- A row opens the agent in its workspace. An archived agent opens with **This agent is
  archived** and **Unarchive** in place of the composer.
- Right-click adds **Unarchive**, **Restore Workspace** (when its workspace can come
  back) and **Delete Permanently**, or **Archive** for an active agent.
- History reloads each time it is opened, not live.

### Command palette

Zed's palette is the only one, on `ctrl-shift-p` and the keys below. Before you type,
it lists the most recently active agents above its commands. Typing finds agents
(**Agent: title — project · status**), the current agent's terminals, and Paseo's
commands under their own names, such as **New terminal**, **Copy agent ID**, **Change
thinking effort**, **Reconnect to host** and **Group sidebar by project or status**.
Typing "project" also finds Zed's commands that Zaseo calls "folder". The commands
removed in [Removed or changed](#removed-or-changed) are hidden.

### Daemon status

The host menu's **Daemon Status** shows the version, PID, listen address, relay, and
each provider's availability. From there you can refresh providers, run a provider's
diagnostic, and restart or update the daemon. Paseo Desktop's own daemon updates only
through Paseo Desktop.

### Agent edits in editors

Unreviewed edits from every agent in the project show inline in open editors until
kept or rejected, like Zed's own agent.

- Each hunk has **Keep** and **Reject**; Reject restores the text before it. The
  toolbar has **Keep All**, **Reject All**, and hunk navigation.
- Your own typing isn't highlighted, and an agent edit you undo by hand is dropped.
- Kept or rejected edits stay reviewed after a restart.
- Whole-file writes aren't highlighted, since the text they replaced is unknown, and
  neither is a pure deletion the daemon can't place.
- While a file shows agent edits, its git hunks are hidden.
- Only local projects with agents on this machine are covered.

## Keyboard shortcuts

`ctrl` on Linux and Windows, `cmd` on macOS.

| Keys | Action |
| --- | --- |
| `ctrl-alt-1` (or `ctrl-alt-p`) | Show or hide the agents list |
| `ctrl-alt-2` | Show or hide the chat panel (`ctrl-b` toggles the left dock, where it starts) |
| `ctrl-alt-3` | Show or hide the editor |
| `ctrl-alt-4` (or `ctrl-alt-b`) | Show or hide the right dock |
| `ctrl-n` (or `ctrl-alt-n`) | New workspace; Zed's New File stays in the command palette |
| `ctrl-t` (or `ctrl-alt-t`) | New agent in the shown workspace; Zed's project symbols stay in the command palette |
| `ctrl-shift-p` | Command palette, with agents first |
| `ctrl-alt-]` / `ctrl-alt-[` | Next / previous agent |
| `ctrl-1` … `ctrl-9` | Open the sidebar row at that position: a workspace, or an agent outside one (macOS: replaces `cmd-1` … `cmd-9` pane focus) |
| `enter` / `shift-enter` | Send (steers a running agent) / new line |
| `ctrl-enter` | Queue the message until the agent finishes |
| `escape` | Interrupt the running agent |
| `shift-tab` | Cycle the permission mode |
| `ctrl-/` | Choose the model |
| `ctrl-alt-/` | Choose the permission mode |
| `ctrl-l` | Focus the composer |
| `shift-alt-a` / `shift-alt-x` | Accept / deny the pending permission |
| `ctrl-shift-backspace`, or `ctrl-backspace` in the sidebar | Archive the agent |
| `ctrl-shift-down` | Scroll to the latest message |
| `ctrl-f` in the sidebar | Filter agents |
| `ctrl-=` / `ctrl--` / `ctrl-0` | Zoom the chat and editors in / out / reset |
| `f2` | Rename the agent |
| `ctrl-alt-shift-e` | Open the Last turn tab |
| `ctrl-shift-t` in Paseo views | New terminal in the agent's directory |
| `ctrl-d` in the composer | Start or stop dictation |
| `ctrl->` in the editor (`ctrl-shift-.` on Windows) | Add the selection to the agent |
| `alt-y` or `ctrl-alt-y` / `ctrl-alt-z` on an agent edit (`cmd-y` or `cmd-alt-y` / `cmd-alt-z` on macOS) | Keep / reject it |
| `shift-alt-y` / `shift-alt-z` on a file with agent edits | Keep / reject all of them |

## Settings

The settings window's **Paseo** page, or the `paseo` key in `settings.json`, controls
hosts, chats, the sidebar, alerts and usage. The defaults and their meaning are in
`assets/settings/default.json` under `paseo`:

- `profiles` and `active_profile`: see [Profiles](#profiles).
- `chat`: prose `font_family`, `font_size` (unset uses Zed's label size, seven eighths of
  the UI font size; editor zoom zooms it), `line_height`, `line_length` (characters),
  whether finished turns start folded (`fold_finished_turns`), whether thinking shows
  while an agent works (`show_thinking`), and `send_behavior` (`"steer"` or
  `"interrupt"`).
- `sidebar`: `grouping` (`"project"`, `"status"` or `"labels"`; the grouping menu sets it
  too), `title_lines`, and whether sidebar rows and tabs animate (`animate_status`): the
  activity line, and a row easing in or flashing when its agent appears or changes
  state.
- `alerts`: whether agents raise `toasts`, whether the bell shows its count
  (`bell_count`), and whether agents raise `system_notifications` while no Zaseo window
  has focus (clicking one opens the agent).
- `usage.display_as`: whether usage percentages show the share `"used"` or
  `"remaining"`.

The default theme is **Paseo Dark** / **Paseo Light**.

## Terms

Zaseo uses Paseo's words for Paseo things. Zed's own panels and commands say "folder"
where Zed says "project".

| Word | In the Paseo sidebar and chats | In Zed's own panels |
| --- | --- | --- |
| Project | A folder or repository added to Paseo. Sidebar rows group by it. | Not used. Zed's set of open folders is called "folders", and Paseo text calls it "the editor". |
| Workspace | A set of agent tabs working in one folder of a project: its root or a Paseo worktree. Several workspaces can share a folder. **Open in Editor** opens its folder. | Not shown by that name. |
| Worktree | A git worktree Paseo made for a workspace, under `~/.paseo/worktrees/`. | One root folder of the editor's open folders. |
| Agent | One chat with one provider, in one workspace. It opens as a tab. | — |
| Subagent | An agent another agent started. It opens as a read-only tab. | — |

## How workspaces map to windows

- A workspace with one agent shows as one sidebar row with the workspace's name, and
  that agent's tab uses the same name. Renaming either renames the workspace.
- Each set of open folders shows the agent tabs of one Paseo workspace at a time, with
  a tab for each of its agents except those another agent started.
- A tab you close stays closed until you open that agent again or restart Zaseo.
- Clicking an agent or a workspace in the sidebar switches to its folder, opening it if
  needed, and shows its workspace's tabs. The tabs of other workspaces on the same
  folder are hidden until their workspace is shown again; they keep unsent text while
  Zaseo runs.
- Open files, splits, and terminals belong to the folder, so every workspace on it
  shares them.
- When another window already shows the folder, that window comes forward. If the
  folder can't be opened, Zaseo shows why and stays where you are.
- **New workspace** (the sidebar's top row, or **New Paseo Workspace** in the menu)
  starts a new workspace with a new agent, as do a project's new-agent entries.
- **New agent** adds an agent to the workspace being shown, like Paseo's new tab.
- Archiving a workspace closes its tabs, and closes its folder too when it was a Paseo
  worktree.
- To see two folders side by side, give each its own window.
- Chats opened while no folder is open go to their own folder when you switch away.
  Drafts without an agent yet go to the folder you switch to.

## Building from source

Follow [Building Zed for Linux](./src/development/linux.md), then run:

```sh
cargo run -p zed
```

If startup fails with `NoWaylandLib`, add the Wayland library directory first:

```sh
export LD_LIBRARY_PATH="$(pkg-config --variable=libdir wayland-client):$LD_LIBRARY_PATH"
```

- **Linux:** `script/install-linux` builds a release bundle and installs it as
  `~/.local/bin/zaseo` with a desktop entry. `script/install.sh` installs an existing
  bundle from `ZASEO_BUNDLE_PATH` and refuses to replace another installation's
  `zaseo` link.
- **macOS:** `script/bundle-mac -i` builds and installs into `/Applications`.
- `script/uninstall.sh` removes the installed app on either system but keeps your
  settings and data.

Release builds come from the `zaseo_release` workflow when a `v*` tag is pushed. It
builds Linux x86_64 and Apple silicon only; the macOS build is not notarized.

### Nix

`nix build .#zaseo` builds a stable release package with the `zaseo` command. From
another flake, add this repository as an input and use
`inputs.zaseo.packages.${system}.zaseo`, or apply `inputs.zaseo.overlays.default` and
use `pkgs.zaseo`. It installs next to nixpkgs' `zed-editor`, and has no `zeditor`
alias.

Flakes only see files tracked by git, so commit new source files before
`nix build .#zaseo` can include them.

## Upstream versions

The current Zed and Paseo releases are listed in the
[README](../README.md#upstream-versions).

Zaseo follows Paseo v0.11.0. It was tested against a v0.10.3 daemon, which covers the
usage list that older daemons send; the per-account usage stream from v0.11 daemons is
covered by tests against a simulated daemon.

Resyncs move to stable release tags only. A Zed resync moves the Zed base and the
remote server pin (`UPSTREAM_REMOTE_SERVER_TAG` in
`crates/remote/src/transport.rs`) together, so Zaseo and its SSH remote server stay on
one Zed release.

## Licensing checks

License information for third-party dependencies must be correct for CI to pass. Zed
uses [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to comply with open
source licenses. If the check fails:

- `no license specified` for a crate you created: set `publish.workspace = true` under
  `[package]` in its `Cargo.toml`, as the Paseo crates do, and give the crate a
  `LICENSE-GPL` or `LICENSE-APACHE` symlink.
- `failed to satisfy license requirements` for a dependency: find the project's
  license and confirm this system can comply with it. If you're unsure, ask a lawyer.
  Then add the license's SPDX identifier to the `accepted` array in
  `script/licenses/zed-licenses.toml`.
- `cargo-about` can't find a dependency's license: add a clarification field at the end
  of `script/licenses/zed-licenses.toml`, as the
  [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration)
  describes.
