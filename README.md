# Zaseo

Zaseo is a fork of the [Zed](https://zed.dev) editor built around
[Paseo](https://paseo.sh) agents. Agents from every host share one native sidebar and
open as tabs next to your code.

- **Agents first.** Projects open from agents, so `zaseo` takes `zaseo://` links, not
  file or folder paths.
- **Installs next to Zed.** It has its own command, links, and data directory, and does
  not update itself.
- **No Zed cloud.** Zed's AI, sign-in, and collaboration are gone; telemetry is off by
  default.

The full guide is [docs/zaseo.md](./docs/zaseo.md).

## Install

Download a build from
[GitHub Releases](https://github.com/saanuregh/zaseo/releases/latest).

- **Linux x86_64:** install the tarball as `~/.local/bin/zaseo` with this repository's
  script:

  ```sh
  ZASEO_BUNDLE_PATH=~/Downloads/zaseo-linux-x86_64.tar.gz script/install.sh
  ```

- **macOS (Apple silicon):** open `Zaseo-aarch64.dmg` and drag Zaseo to Applications.
  The build is not notarized, so run this once before the first launch:

  ```sh
  xattr -dr com.apple.quarantine /Applications/Zaseo.app
  ```

- **Nix:** `nix build .#zaseo`.

To build from source or use the flake from another flake, see
[Building from source](./docs/zaseo.md#building-from-source).

## Connect to Paseo

Install and start a Paseo daemon; Zaseo does not bundle one. Zaseo connects to
`ws://127.0.0.1:6767/ws` by default. Add more hosts from the host menu at the top of
the sidebar. See [Connections](./docs/zaseo.md#connections) for remote daemons and
SSH, and [Keyboard shortcuts](./docs/zaseo.md#keyboard-shortcuts) to get around.

## Upstream versions

| Project | Release |
| --- | --- |
| Zed | [`v1.23.2`](https://github.com/zed-industries/zed/releases/tag/v1.23.2) |
| Zed remote server | [`v1.23.2`](https://github.com/zed-industries/zed/releases/tag/v1.23.2), installed on SSH hosts; its protocol matches the Zed base |
| Paseo | [`v0.11.0`](https://github.com/getpaseo/paseo/releases/tag/v0.11.0), protocol v1 |

See [Upstream versions](./docs/zaseo.md#upstream-versions) for compatibility and
resync notes.

## License

Zaseo is built on [Zed](https://github.com/zed-industries/zed) by Zed Industries, Inc.,
and is licensed mainly under GPL-3.0-or-later, with Apache-2.0 components where marked.
The Paseo themes come from Paseo under Apache-2.0; see `assets/themes/LICENSES`.
