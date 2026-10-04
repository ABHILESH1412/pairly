# Build and install Pairly for the current user (no root needed).
#
#   make install-user     release build, install under ~/.local, enable and (re)start pairlyd
#   make uninstall-user   remove it again (your pairings in ~/.local/share/pairly are kept)

PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
UNITDIR ?= $(DATADIR)/systemd/user
DBUSDIR ?= $(DATADIR)/dbus-1/services

# Parallel compile jobs. Each rustc can use 1-2 GB on the GTK and zbus crates; raise this only
# with RAM to spare (make install-user JOBS=8).
JOBS ?= 4

FM      := data/file-managers
NAUTILUS_SCRIPT := $(DATADIR)/nautilus/scripts/Send to Device
DOLPHIN_MENU    := $(DATADIR)/kio/servicemenus/pairly-send.desktop
NEMO_ACTION     := $(DATADIR)/nemo/actions/pairly-send.nemo_action

APP_ID   := dev.pairly.Pairly
BINARIES := pairlyd pairly pairly-gtk

.PHONY: build install-user uninstall-user

build:
	cargo build --release --locked -j $(JOBS)

install-user: build
	install -Dm755 -t $(BINDIR) $(addprefix target/release/,$(BINARIES))
	install -Dm644 data/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/icons/$(APP_ID).svg $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/icons/$(APP_ID)-symbolic.svg $(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
	install -d $(UNITDIR) $(DBUSDIR)
	sed 's|@bindir@|$(BINDIR)|g' data/pairlyd.service.in > $(UNITDIR)/pairlyd.service
	sed 's|@bindir@|$(BINDIR)|g' data/dev.pairly.Daemon.service.in > $(DBUSDIR)/dev.pairly.Daemon.service
	install -Dm755 $(FM)/nautilus-send-to-device "$(NAUTILUS_SCRIPT)"
	install -Dm755 $(FM)/pairly-send.dolphin.desktop $(DOLPHIN_MENU)
	install -Dm644 $(FM)/pairly-send.nemo_action $(NEMO_ACTION)
	-gtk-update-icon-cache -qtf $(DATADIR)/icons/hicolor
	-update-desktop-database -q $(DATADIR)/applications
	-busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig
	systemctl --user daemon-reload
	systemctl --user enable pairlyd.service
	systemctl --user restart pairlyd.service

uninstall-user:
	-systemctl --user disable --now pairlyd.service
	rm -f $(addprefix $(BINDIR)/,$(BINARIES))
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
	rm -f $(UNITDIR)/pairlyd.service $(DBUSDIR)/dev.pairly.Daemon.service
	rm -f "$(NAUTILUS_SCRIPT)" $(DOLPHIN_MENU) $(NEMO_ACTION)
	-systemctl --user daemon-reload
