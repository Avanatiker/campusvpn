#!/bin/sh
# Installs (or with --uninstall removes) the latest eduvpn-rs release for the current user.
#   curl -fsSL https://raw.githubusercontent.com/Avanatiker/eduvpn-rs/master/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Avanatiker/eduvpn-rs/master/install.sh | sh -s -- --uninstall
set -eu

REPO="Avanatiker/eduvpn-rs"
APP_ID="io.github.avanatiker.eduvpn-rs"
PREFIX="${PREFIX:-$HOME/.local}"

BIN="$PREFIX/bin/eduvpn-rs"
LIB_DIR="$PREFIX/lib/eduvpn-rs"
DESKTOP="$PREFIX/share/applications/$APP_ID.desktop"
ICON="$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"

# Make menus and docks pick up the new launcher and icon without logging out.
refresh_caches() {
    touch "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    # Only update an existing GTK icon cache: creating one hides icons it does not list.
    if [ -f "$PREFIX/share/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" || true
    fi
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database -q "$PREFIX/share/applications" || true
    fi
    for k in kbuildsycoca6 kbuildsycoca5; do
        if command -v "$k" >/dev/null; then "$k" >/dev/null 2>&1 || true; break; fi
    done
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN" "$DESKTOP" "$ICON" "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$APP_ID.desktop"
    rm -rf "$LIB_DIR"
    refresh_caches
    echo "eduvpn-rs removed. Settings are kept in ~/.config/eduvpn-rs."
    exit 0
fi

[ "$(uname -m)" = "x86_64" ] || { echo "Only x86_64 builds are available, build from source instead." >&2; exit 1; }
command -v nmcli >/dev/null || echo "Warning: NetworkManager (nmcli) not found; it is required." >&2

# The latest release URL redirects to .../tag/vX.Y.Z
tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
tag=${tag##*/}
case "$tag" in v*) ;; *) echo "Could not find the latest release." >&2; exit 1 ;; esac
name="eduvpn-rs-${tag#v}-x86_64-linux"
url="https://github.com/$REPO/releases/download/$tag/$name.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading eduvpn-rs $tag ..."
curl -fsSL -o "$tmp/$name.tar.gz" "$url"
curl -fsSL -o "$tmp/$name.tar.gz.sha256" "$url.sha256"
(cd "$tmp" && sha256sum -c --quiet "$name.tar.gz.sha256")
tar xzf "$tmp/$name.tar.gz" -C "$tmp"

install -Dm755 "$tmp/$name/eduvpn-rs" "$BIN"
mkdir -p "$LIB_DIR"
rm -f "$LIB_DIR"/libeduvpn_common-*.so
install -m755 "$tmp/$name"/libeduvpn_common-*.so "$LIB_DIR/"
install -Dm644 "$tmp/$name/$APP_ID.svg" "$ICON"
mkdir -p "$(dirname "$DESKTOP")"
# Absolute path, so the menu entry works even if ~/.local/bin is not in PATH.
sed "s|^Exec=eduvpn-rs|Exec=$BIN|" "$tmp/$name/$APP_ID.desktop" > "$DESKTOP"
refresh_caches

echo "Installed eduvpn-rs $tag to $PREFIX. Start it from your app menu or run: $BIN"
