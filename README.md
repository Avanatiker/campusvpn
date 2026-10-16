# eduvpn-rs

A small eduVPN client for Linux, written in Rust with GTK4/libadwaita.

- **System tray**: status icon with connect/disconnect, server switching, and show/quit.
  Closing the window keeps it running in the tray.
- **Auto-connect** to the last used server on start, after the network is up.
- **Start on login** (optionally hidden in the tray).
- WireGuard, OpenVPN and WireGuard over TCP (ProxyGuard). If UDP is blocked it falls back to TCP.
- Profile and Secure Internet location switching, session renewal.

The protocol work (discovery, OAuth login, token refresh, fetching VPN configs) is done by
the official [eduvpn-common](https://codeberg.org/eduVPN/eduvpn-common) Go library, loaded
at runtime, the same way the official Python client does. The VPN itself is created through
NetworkManager.

## Requirements

- NetworkManager (plus `networkmanager-openvpn` for OpenVPN servers)
- GTK 4.14+, libadwaita 1.5+
- `libeduvpn_common-5.x.so`. On Arch it comes with `python-eduvpn-common`; it is found
  automatically in the usual library dirs, Python site-packages, `~/.local/lib/eduvpn-rs/`
  or next to the binary. Override with `EDUVPN_COMMON_LIB=/path/to/lib.so`.
- A Secret Service (KWallet or GNOME Keyring) for tokens. Without one, tokens go to
  `~/.config/eduvpn/keys` (mode 600).
- For the tray on GNOME: the AppIndicator extension. KDE, Xfce, Cinnamon etc. work out of the box.

## Build and install

```sh
make install          # installs to ~/.local (binary, desktop entry, icon)
eduvpn-rs             # or start "eduVPN" from the app menu
```

Enable "Start on login" in Settings for autostart.

## Compatibility with the official client

It uses the same config dir (`~/.config/eduvpn`), client ID and keyring entries as the
official Python client, so your servers and logins carry over. Do not run both at the same time.

## Options

```
eduvpn-rs --hidden     start in the tray only
RUST_LOG=debug eduvpn-rs
```

## Tests

`cargo test -- --ignored` runs tests that add (but never activate) dummy WireGuard and
OpenVPN connections in NetworkManager and remove them again.
