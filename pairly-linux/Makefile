# Build and install Pairly.
#
#   make install-user     release build, install under ~/.local, enable and (re)start pairlyd
#   make uninstall-user   remove it again (your pairings in ~/.local/share/pairly are kept)
#
# For packages (files only, nothing started):
#   make build && make install DESTDIR="$pkgdir" PREFIX=/usr UNITDIR=/usr/lib/systemd/user

PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
UNITDIR ?= $(DATADIR)/systemd/user
DBUSDIR ?= $(DATADIR)/dbus-1/services
DESTDIR ?=

# Parallel compile jobs. Each rustc can use 1-2 GB on the GTK and zbus crates; raise this only
# with RAM to spare (make install-user JOBS=8).
JOBS ?= 4

FM      := data/file-managers
NAUTILUS_SCRIPT := $(DATADIR)/nautilus/scripts/Send to Device
DOLPHIN_MENU    := $(DATADIR)/kio/servicemenus/pairly-send.desktop
NEMO_ACTION     := $(DATADIR)/nemo/actions/pairly-send.nemo_action

APP_ID   := io.github.abhilesh1412.Pairly
BINARIES := pairlyd pairly pairly-gtk

.PHONY: build install install-user uninstall-user

build:
	cargo build --release --locked -j $(JOBS)

# Files only, under $(DESTDIR); the service files point at the final $(BINDIR).
install:
	install -Dm755 -t $(DESTDIR)$(BINDIR) $(addprefix target/release/,$(BINARIES))
	install -Dm644 data/$(APP_ID).desktop $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/$(APP_ID).svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/icons/$(APP_ID)-symbolic.svg $(DESTDIR)$(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
	install -d $(DESTDIR)$(UNITDIR) $(DESTDIR)$(DBUSDIR)
	sed 's|@bindir@|$(BINDIR)|g' data/pairlyd.service.in > $(DESTDIR)$(UNITDIR)/pairlyd.service
	sed 's|@bindir@|$(BINDIR)|g' data/$(APP_ID).Daemon.service.in > $(DESTDIR)$(DBUSDIR)/$(APP_ID).Daemon.service
ifeq ($(PREFIX),$(HOME)/.local)
	# Nautilus only reads scripts from the user's own folder.
	install -Dm755 $(FM)/nautilus-send-to-device "$(DESTDIR)$(NAUTILUS_SCRIPT)"
endif
	install -Dm755 $(FM)/pairly-send.dolphin.desktop $(DESTDIR)$(DOLPHIN_MENU)
	install -Dm644 $(FM)/pairly-send.nemo_action $(DESTDIR)$(NEMO_ACTION)

install-user: build install
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
	rm -f $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
	rm -f $(UNITDIR)/pairlyd.service $(DBUSDIR)/$(APP_ID).Daemon.service
	rm -f "$(NAUTILUS_SCRIPT)" $(DOLPHIN_MENU) $(NEMO_ACTION)
	-systemctl --user daemon-reload
