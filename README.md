> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zaseo

Zaseo is a fork of Zed that shows [Paseo](https://paseo.sh) agents in native
panels and tabs. Zed's built-in AI features are always off. Zaseo uses its own
`zaseo` command, `zaseo://` links, and data directory, so it can be installed
next to Zed. It does not update itself.

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

## Connect to Paseo

Zaseo connects to a Paseo daemon you install and start yourself. It does not
bundle or start Paseo. It targets Paseo at upstream commit `8cd9895` and shows
an incompatibility error for older daemons. Open **View → Paseo Panel** or **View → Paseo Tab**. Then
pick a connection profile or add one. The default `Local` profile points to
`ws://127.0.0.1:6767/ws`.

Profiles are stored under `paseo.profiles` in your settings:

- `target_uri`: a `ws://` or `wss://` daemon URL, or
  `ssh://user@host:port?daemonPort=6767` to reach a remote daemon's loopback
  port through `ssh -W`.
- `editor_ssh_uri` (optional, direct URLs only): the SSH host that **Open
  Workspace** uses to open an agent's directory remotely. SSH profiles use their
  own target for this.

SSH editing (including **Open Workspace** for remote agents) needs a remote
server on the host. Builds run with `cargo run` compile and upload one. Installed
release builds do not download Zed's server, so SSH projects fail unless a
matching server is already in the host's `~/.zed_server`.

Zaseo keeps passwords in memory only and never saves them. It rejects URLs that
contain credentials. A remote `ws://` connection is only as private as the
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
