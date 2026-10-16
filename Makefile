PREFIX ?= $(HOME)/.local
BIN = target/release/eduvpn-rs

.PHONY: build install uninstall

build:
	cargo build --release

install: build
	install -Dm755 $(BIN) $(PREFIX)/bin/eduvpn-rs
	install -Dm644 data/org.eduvpn.rs.desktop $(PREFIX)/share/applications/org.eduvpn.rs.desktop
	install -Dm644 data/icons/org.eduvpn.rs.svg $(PREFIX)/share/icons/hicolor/scalable/apps/org.eduvpn.rs.svg
	-update-desktop-database $(PREFIX)/share/applications 2>/dev/null

uninstall:
	rm -f $(PREFIX)/bin/eduvpn-rs $(PREFIX)/share/applications/org.eduvpn.rs.desktop \
		$(PREFIX)/share/icons/hicolor/scalable/apps/org.eduvpn.rs.svg
