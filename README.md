# eduvpn-rs

An unofficial, lightweight [eduVPN](https://www.eduvpn.org/) client for Linux, written in
Rust with GTK 4 and libadwaita.

> **Not affiliated with eduVPN, GÉANT or SURF.** eduVPN is a service of GÉANT and its
> partners; this is an independent client. For the official app see
> [codeberg.org/eduVPN/linux-app](https://codeberg.org/eduVPN/linux-app).

## Features

- **System tray**: status icon with connect/disconnect, server switching, show and quit.
  Closing the window keeps the app running in the tray.
- **Auto-connect** to the last used server on start, once the network is up.
- **Start on login**, optionally hidden in the tray.
- WireGuard, OpenVPN and WireGuard over TCP (ProxyGuard).
- Profile and Secure Internet location switching, session renewal.

The protocol work (server discovery, OAuth login, token refresh, fetching VPN configs) is done
by the official [eduvpn-common](https://codeberg.org/eduVPN/eduvpn-common) library, loaded at
runtime, as the official client does. The VPN itself is set up through NetworkManager.

## Install

### Arch Linux (AUR)

```sh
paru -S eduvpn-rs      # or yay, or makepkg from https://aur.archlinux.org/packages/eduvpn-rs
```

The package builds eduvpn-common itself; no Python is needed.

### From source

Requirements:

- Rust 1.92+, GTK 4.14+, libadwaita 1.5+
- NetworkManager, plus `networkmanager-openvpn` for OpenVPN servers
- `libeduvpn_common-5.x.so`. Build it from
  [eduvpn-common](https://codeberg.org/eduVPN/eduvpn-common) with `make` (needs Go), or use
  the one shipped with the `python-eduvpn_common` package. It is found in `/usr/lib/eduvpn-rs/`,
  the usual library dirs, Python site-packages, `~/.local/lib/eduvpn-rs/` or next to the
  binary. Override with `EDUVPN_COMMON_LIB=/path/to/libeduvpn_common-5.x.so`.

```sh
make install           # installs to ~/.local (binary, desktop entry, icon)
eduvpn-rs              # or start "eduvpn-rs" from the app menu
```

Enable "Start on login" in Settings for autostart.

### Desktop notes

- Tokens are stored in the Secret Service (KWallet, GNOME Keyring). Without one they are
  written to `~/.config/eduvpn/keys` with mode 600.
- The tray uses StatusNotifierItem: works on KDE, Xfce, Cinnamon, etc. On GNOME install the
  AppIndicator extension. Without a tray, closing the window quits the app.

## Compatibility with the official client

eduvpn-rs uses the same config directory (`~/.config/eduvpn`), OAuth client ID and keyring
entries as the official Linux client, so servers and logins carry over in both directions.
Don't run both at the same time.

## Usage

```
eduvpn-rs              open the window
eduvpn-rs --hidden     start in the tray only
RUST_LOG=debug eduvpn-rs
```

## Development

```sh
cargo test                 # unit tests
cargo test -- --ignored    # adds (never activates) dummy WireGuard/OpenVPN connections
                           # in NetworkManager and removes them again
```

## License

GPL-3.0-or-later. Parts of the connection logic follow the official eduVPN Linux client
(GPL-3.0-or-later). eduvpn-common, built into the AUR package, is MIT licensed.
The eduVPN logo is not included: it is reserved for the official eduVPN applications.
