#!/usr/bin/env sh
set -eu

fail() {
    printf '%s\n' "$1" >&2
    exit 1
}

[ "$(uname -s)" = "Linux" ] || fail "Use script/bundle-mac -i for a local macOS build."

bundle=${ZASEO_BUNDLE_PATH:-}
[ -n "$bundle" ] && [ -f "$bundle" ] || fail "Set ZASEO_BUNDLE_PATH to a locally built Zaseo archive."

channel=${ZASEO_CHANNEL:-stable}
case "$channel" in
    stable) suffix=; app_id=local.zaseo.Zaseo ;;
    preview) suffix=-preview; app_id=local.zaseo.Zaseo-Preview ;;
    nightly) suffix=-nightly; app_id=local.zaseo.Zaseo-Nightly ;;
    dev) suffix=-dev; app_id=local.zaseo.Zaseo-Dev ;;
    *) fail "Unknown Zaseo channel: $channel" ;;
esac

app_dir="zaseo${suffix}.app"
entries=$(tar -tzf "$bundle") || fail "Zaseo archive cannot be read."
printf '%s\n' "$entries" | awk -v root="$app_dir" '
    {
        if ($0 != root && index($0, root "/") != 1) exit 1
        count = split($0, parts, "/")
        for (i = 1; i <= count; i++) {
            if (parts[i] == "..") exit 1
        }
    }
' || fail "Zaseo archive contains an unexpected path."

# Staging beside the install location keeps the swap below to same-filesystem
# renames, so a failed move cannot leave a partial app behind.
mkdir -p "$HOME/.local"
staging=$(mktemp -d "$HOME/.local/.zaseo-install.XXXXXX") || fail "Cannot create staging directory."
trap 'rm -rf "$staging"' 0
tar -xzf "$bundle" -C "$staging" || fail "Cannot extract Zaseo archive."

source_app="$staging/$app_dir"
for required in "bin/zaseo" "libexec/zaseo-editor" \
    "share/applications/$app_id.desktop" \
    "share/icons/hicolor/512x512/apps/zaseo${suffix}.png"; do
    [ -f "$source_app/$required" ] || fail "Zaseo archive is missing $required."
done

install_app="$HOME/.local/$app_dir"
cli_link="$HOME/.local/bin/zaseo"
if [ -e "$cli_link" ] || [ -L "$cli_link" ]; then
    [ -L "$cli_link" ] || fail "$cli_link exists and is not a symlink; refusing to replace it."
    current_link=$(readlink "$cli_link")
    case "$current_link" in
        "$HOME"/.local/zaseo*.app/bin/zaseo) ;;
        *) fail "$cli_link belongs to another installation; refusing to replace it." ;;
    esac
fi
[ ! -L "$install_app" ] || fail "$install_app is a symlink; refusing to replace it."
[ ! -e "$install_app" ] || [ -d "$install_app" ] || fail "$install_app is not a directory."

awk -v cli="$install_app/bin/zaseo" \
    -v icon="$install_app/share/icons/hicolor/512x512/apps/zaseo${suffix}.png" '
    /^TryExec=/ { print "TryExec=" cli; next }
    /^Exec=/ {
        remainder = substr($0, 6)
        sub(/^zaseo/, "", remainder)
        print "Exec=\"" cli "\"" remainder
        next
    }
    /^Icon=/ { print "Icon=" icon; next }
    { print }
' "$source_app/share/applications/$app_id.desktop" > "$staging/$app_id.desktop" || fail "Cannot prepare desktop entry."

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
if [ -d "$install_app" ]; then
    mv "$install_app" "$staging/previous.app" || fail "Cannot move previous Zaseo installation."
fi
if ! mv "$source_app" "$install_app"; then
    if [ -d "$staging/previous.app" ]; then
        mv "$staging/previous.app" "$install_app"
    fi
    fail "Cannot install Zaseo."
fi
ln -sfn "$install_app/bin/zaseo" "$cli_link"
cp "$staging/$app_id.desktop" "$HOME/.local/share/applications/$app_id.desktop"

printf 'Installed Zaseo at %s\nRun %s\n' "$install_app" "$cli_link"
