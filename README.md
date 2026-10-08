> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Zaseo

Zaseo is a fork of [Zed](https://zed.dev) that shows [Paseo](https://paseo.sh) agents
in a native sidebar and agent tabs.

- **Agents first.** Run `zaseo` with no arguments. Projects open from agents, so the
  command takes `zaseo://` links but not file or folder paths. The File menu, title
  bar, and welcome page have no project-opening actions.
- **Installs next to Zed.** Zaseo has its own `zaseo` command, `zaseo://` links, and
  data directory. It does not update itself.
- **Zed's cloud features are gone.** Zed's built-in AI features are always off. Sign-in
  and collaboration are removed. Telemetry is off by default, because it would go to
  Zed's servers.

The full feature guide, shortcuts, settings, and terms are in
[docs/zaseo.md](./docs/zaseo.md).

## Install

### Linux

Follow [Building Zed for Linux](./docs/src/development/linux.md), then run:

```sh
cargo run -p zed
```

If startup fails with `NoWaylandLib`, add the Wayland library directory first:

```sh
export LD_LIBRARY_PATH="$(pkg-config --variable=libdir wayland-client):$LD_LIBRARY_PATH"
```

To build a release bundle and install it as `~/.local/bin/zaseo`, run
`script/install-linux`. `script/uninstall.sh` removes the installed app but keeps your
settings and data.

### Nix

The flake builds a stable release package with the `zaseo` command:

```sh
nix build .#zaseo
```

To install it from another flake, add this repository as an input and use
`inputs.zaseo.packages.${system}.zaseo`, or apply `inputs.zaseo.overlays.default` and
use `pkgs.zaseo`. It installs next to nixpkgs' `zed-editor`.

Flakes only see files tracked by git, so new source files must be committed before
`nix build .#zaseo` includes them.

## Connect to Paseo

1. Install and start a Paseo daemon yourself. Zaseo does not bundle or start Paseo.
2. Start Zaseo. It connects to every configured host at once and lists all their agents
   in one sidebar. The default `Local` profile points to `ws://127.0.0.1:6767/ws`.
3. To add a host, or pick the default host for new agents, open the host menu at the
   top of the Paseo sidebar, or **Manage Hosts…**. The sidebar's filter menu can show
   only some hosts.

Zaseo shows an incompatibility error when a daemon lacks the features it needs. For
remote daemons, SSH, and passwords, see
[Connections](./docs/zaseo.md#connections).

## Getting around

| Keys (`cmd` on macOS) | Action |
| --- | --- |
| `ctrl-alt-1` … `ctrl-alt-4` | Show or hide the agents list, chat panel, editor, or right dock |
| `ctrl-n` (or `ctrl-alt-n`) | New workspace |
| `ctrl-t` (or `ctrl-alt-t`) | New agent in the shown workspace |
| `ctrl-shift-p` | Command palette, with agents first |
| `ctrl-alt-]` / `ctrl-alt-[` | Next / previous agent |
| `enter` / `shift-enter` | Send / new line |
| `escape` | Interrupt the running agent |

See [Keyboard shortcuts](./docs/zaseo.md#keyboard-shortcuts) for the full list.

## Upstream versions

| Project | Release |
| --- | --- |
| Zed | [`v1.23.2`](https://github.com/zed-industries/zed/releases/tag/v1.23.2) |
| Zed remote server | [`v1.23.2`](https://github.com/zed-industries/zed/releases/tag/v1.23.2), installed on SSH hosts; its protocol matches the Zed base |
| Paseo | [`v0.11.0`](https://github.com/getpaseo/paseo/releases/tag/v0.11.0), protocol v1 |

Zaseo's UI and protocol follow Paseo v0.11.0. It was tested against a v0.10.3 daemon,
which exercises the usage list older daemons send. The per-account usage stream that
v0.11 daemons send is covered by tests against a simulated daemon. Future resyncs move both projects to stable release tags only. Each Zed resync
moves the Zed base and the remote server pin
(`UPSTREAM_REMOTE_SERVER_TAG` in `crates/remote/src/transport.rs`) together,
so Zaseo and its SSH remote server stay on one Zed release.

## Based on Zed

Zaseo is built on [Zed](https://github.com/zed-industries/zed), a code editor by Zed
Industries, Inc. For Zed itself, see [zed.dev](https://zed.dev).

## License

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0
components where marked. The Paseo Dark and Paseo Light themes come from Paseo under
Apache-2.0; see `assets/themes/LICENSES`. For CI license checks, see
[Licensing checks](./docs/zaseo.md#licensing-checks).
