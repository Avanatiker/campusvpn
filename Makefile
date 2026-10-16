PREFIX ?= $(HOME)/.local
BIN = target/release/eduvpn-rs

.PHONY: build install uninstall

build:
	cargo build --release

install: build
	install -Dm755 $(BIN) $(PREFIX)/bin/eduvpn-rs
	install -Dm644 data/io.github.avanatiker.eduvpn-rs.desktop $(PREFIX)/share/applications/io.github.avanatiker.eduvpn-rs.desktop
	install -Dm644 data/icons/io.github.avanatiker.eduvpn-rs.svg $(PREFIX)/share/icons/hicolor/scalable/apps/io.github.avanatiker.eduvpn-rs.svg
	-update-desktop-database $(PREFIX)/share/applications 2>/dev/null

uninstall:
	rm -f $(PREFIX)/bin/eduvpn-rs $(PREFIX)/share/applications/io.github.avanatiker.eduvpn-rs.desktop \
		$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.avanatiker.eduvpn-rs.svg
