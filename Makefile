PREFIX ?= $(HOME)/.local
BIN = target/release/campusvpn

.PHONY: build install uninstall

build:
	cargo build --release

install: build
	install -Dm755 $(BIN) $(PREFIX)/bin/campusvpn
	install -Dm644 data/io.github.avanatiker.campusvpn.desktop $(PREFIX)/share/applications/io.github.avanatiker.campusvpn.desktop
	install -Dm644 data/icons/io.github.avanatiker.campusvpn.svg $(PREFIX)/share/icons/hicolor/scalable/apps/io.github.avanatiker.campusvpn.svg
	-touch $(PREFIX)/share/icons/hicolor
	-update-desktop-database -q $(PREFIX)/share/applications 2>/dev/null
	-kbuildsycoca6 >/dev/null 2>&1

uninstall:
	rm -f $(PREFIX)/bin/campusvpn $(PREFIX)/share/applications/io.github.avanatiker.campusvpn.desktop \
		$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.avanatiker.campusvpn.svg
