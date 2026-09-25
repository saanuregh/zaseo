#!/usr/bin/env sh
set -eu

fail() {
    printf '%s\n' "$1" >&2
    exit 1
}

# Match script/install-linux, which installs the checkout's release channel.
release_channel_file="$(dirname "$0")/../crates/zed/RELEASE_CHANNEL"
default_channel=stable
if [ -f "$release_channel_file" ]; then
    default_channel=$(cat "$release_channel_file")
fi
channel=${ZASEO_CHANNEL:-$default_channel}
case "$channel" in
    stable) suffix=; app_id=local.zaseo.Zaseo; bundle_name=Zaseo.app ;;
    preview) suffix=-preview; app_id=local.zaseo.Zaseo-Preview; bundle_name='Zaseo Preview.app' ;;
    nightly) suffix=-nightly; app_id=local.zaseo.Zaseo-Nightly; bundle_name='Zaseo Nightly.app' ;;
    dev) suffix=-dev; app_id=local.zaseo.Zaseo-Dev; bundle_name='Zaseo Dev.app' ;;
    *) fail "Unknown Zaseo channel: $channel" ;;
esac

case "$(uname -s)" in
    Linux)
        app="$HOME/.local/zaseo${suffix}.app"
        desktop="$HOME/.local/share/applications/$app_id.desktop"
        ;;
    Darwin)
        app="/Applications/$bundle_name"
        desktop=
        ;;
    *) fail "Unsupported operating system." ;;
esac

[ ! -L "$app" ] || fail "$app is a symlink; refusing to remove it."
removed=
for cli_link in "$HOME/.local/bin/zaseo" /usr/local/bin/zaseo; do
    [ -L "$cli_link" ] || continue
    case "$(readlink "$cli_link")" in
        "$app"/*)
            rm "$cli_link" || fail "Cannot remove $cli_link; remove it manually."
            removed=1
            ;;
    esac
done
if [ -d "$app" ]; then
    rm -rf "$app"
    removed=1
fi
if [ -n "$desktop" ] && [ -f "$desktop" ]; then
    rm "$desktop"
    removed=1
fi

[ -n "$removed" ] || fail "No $channel Zaseo installation found at $app. Set ZASEO_CHANNEL to choose another channel."
printf 'Removed the %s application files. User settings and data were kept.\n' "$channel"
