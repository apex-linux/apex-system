# SPDX-License-Identifier: GPL-3.0-or-later
#
# GNU make. packaging/apex.spec's %install runs `make install`, so this file alone
# defines the installed layout.

PREFIX      ?= /usr
BINDIR      ?= $(PREFIX)/bin
LIBEXECDIR  ?= $(PREFIX)/libexec/apex
DATADIR     ?= $(PREFIX)/share
UNITDIR     ?= $(PREFIX)/lib/systemd/system
USERUNITDIR ?= $(PREFIX)/lib/systemd/user
USERPRESETDIR ?= $(PREFIX)/lib/systemd/user-preset
TMPFILESDIR ?= $(PREFIX)/lib/tmpfiles.d
DBUSDIR     ?= $(DATADIR)/dbus-1
POLKITDIR   ?= $(DATADIR)/polkit-1/actions
BOOTUPDDIR  ?= $(PREFIX)/lib/bootupd/grub2-static/configs.d
DESTDIR     ?=

CARGO       ?= cargo
INSTALL     ?= install
RPMBUILD    ?= rpmbuild

AUDITABLE        ?= auto
CARGO_BUILD_ARGS ?= --release --locked
# Cargo honours CARGO_TARGET_DIR too, so both agree on where the binaries land.
CARGO_TARGET_DIR ?= target
RELEASE_DIR      := $(CARGO_TARGET_DIR)/release

# --bin, not --workspace or --bins: keeps the GUI crates and their GTK dependency out.
APEX_BINS := apexctl apex-updated apex-boot-assess apex-session-agent

DBUS_IFACE_XML := crates/apex-dbus/data/io.github.apex_linux.Apex.Update1.xml
UPDATED_DATA   := crates/apex-updated/data
BOOT_DATA      := crates/apex-boot/data
AGENT_DATA     := crates/apex-session-agent/data

XML_DATA := $(DBUS_IFACE_XML) \
	$(UPDATED_DATA)/io.github.apex_linux.Apex.Update1.conf \
	$(UPDATED_DATA)/io.github.apex-linux.apex.update.policy
UNITS := apex-updated.service apex-update-auto.service apex-update-auto.timer \
	apex-boot-assess.service
SYSTEMD_UNITS_WITH_APEX_DROPIN := emergency.service rescue.service
CHECK_ROOT := $(abspath $(CARGO_TARGET_DIR))/check-data/root

# The = variables expand lazily, so git and sed run only when a target needs them.
SPEC          := packaging/apex.spec
RPM_TOPDIR    := $(abspath $(CARGO_TARGET_DIR))/rpm
SPEC_VERSION   = $(shell sed -n 's/^Version:[[:space:]]*//p' $(SPEC))
CRATE_VERSION  = $(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml)
COMMITDATE     = $(shell TZ=UTC git log -1 --format=%cd --date=format-local:%Y%m%d HEAD 2>/dev/null)
SHORTCOMMIT    = $(shell git rev-parse --short=12 HEAD 2>/dev/null)
RPMBUILD_FLAGS ?=

.PHONY: all build install install-bin install-data check-data rpm clean help
.DELETE_ON_ERROR:

all: build

help:
	@echo 'make build                release binaries (cargo auditable when it is installed)'
	@echo 'make install DESTDIR=DIR  install the layout; never builds'
	@echo 'make check-data           validate the D-Bus, polkit and systemd data files'
	@echo 'make rpm                  SRPM and RPMs from git HEAD with rpmbuild (packaging/README.md)'
	@echo 'make clean'
	@echo 'Common variables: DESTDIR PREFIX LIBEXECDIR CARGO AUDITABLE=auto|yes|no RPMBUILD_FLAGS'
	@echo 'AUDITABLE=auto            cargo auditable when installed, else plain cargo with a warning'
	@echo 'AUDITABLE=yes             fail when cargo auditable is missing (the RPM build)'
	@echo 'AUDITABLE=no              never use cargo auditable'

build:
	@set -e; \
	bins='$(foreach b,$(APEX_BINS),--bin $(b))'; \
	if [ '$(AUDITABLE)' != no ] && \
		$(CARGO) --list 2>/dev/null | grep -Eq '^[[:space:]]+auditable([[:space:]]|$$)'; then \
		echo "$(CARGO) auditable build $(CARGO_BUILD_ARGS) $$bins"; \
		$(CARGO) auditable build $(CARGO_BUILD_ARGS) $$bins; \
	elif [ '$(AUDITABLE)' = yes ]; then \
		echo "error: AUDITABLE=yes but 'cargo auditable' is not installed" \
			"(cargo install --locked cargo-auditable)" >&2; \
		exit 1; \
	else \
		if [ '$(AUDITABLE)' != no ]; then \
			echo "warning: cargo-auditable not found; the binaries will not embed their" \
				"dependency list" >&2; \
		fi; \
		echo "$(CARGO) build $(CARGO_BUILD_ARGS) $$bins"; \
		$(CARGO) build $(CARGO_BUILD_ARGS) $$bins; \
	fi

# install must not depend on build: rpmbuild runs it in %install, after %build.
install: install-bin install-data

install-bin:
	@for b in $(APEX_BINS); do \
		if [ ! -x '$(RELEASE_DIR)/'"$$b" ]; then \
			echo "error: $(RELEASE_DIR)/$$b is missing; run 'make build' first" >&2; \
			exit 1; \
		fi; \
	done
	$(INSTALL) -Dm0755 $(RELEASE_DIR)/apexctl '$(DESTDIR)$(BINDIR)/apexctl'
	$(INSTALL) -Dm0755 $(RELEASE_DIR)/apex-updated '$(DESTDIR)$(LIBEXECDIR)/apex-updated'
	$(INSTALL) -Dm0755 $(RELEASE_DIR)/apex-boot-assess '$(DESTDIR)$(LIBEXECDIR)/apex-boot-assess'
	$(INSTALL) -Dm0755 $(RELEASE_DIR)/apex-session-agent '$(DESTDIR)$(LIBEXECDIR)/apex-session-agent'

define install_data
	$(INSTALL) -Dm0644 $(DBUS_IFACE_XML) \
		'$(1)$(DBUSDIR)/interfaces/io.github.apex_linux.Apex.Update1.xml'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-updated.service \
		'$(1)$(UNITDIR)/apex-updated.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-update-auto.service \
		'$(1)$(UNITDIR)/apex-update-auto.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-update-auto.timer \
		'$(1)$(UNITDIR)/apex-update-auto.timer'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.apex_linux.Apex.Update1.service \
		'$(1)$(DBUSDIR)/system-services/io.github.apex_linux.Apex.Update1.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.apex_linux.Apex.Update1.conf \
		'$(1)$(DBUSDIR)/system.d/io.github.apex_linux.Apex.Update1.conf'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.apex-linux.apex.update.policy \
		'$(1)$(POLKITDIR)/io.github.apex-linux.apex.update.policy'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex.tmpfiles.conf \
		'$(1)$(TMPFILESDIR)/apex.conf'
	$(INSTALL) -Dm0644 $(BOOT_DATA)/apex-boot-assess.service \
		'$(1)$(UNITDIR)/apex-boot-assess.service'
	$(INSTALL) -Dm0644 $(BOOT_DATA)/08_apex-bootcount.cfg \
		'$(1)$(BOOTUPDDIR)/08_apex-bootcount.cfg'
	$(INSTALL) -Dm0644 $(BOOT_DATA)/emergency.service.d/50-apex-bootcount.conf \
		'$(1)$(UNITDIR)/emergency.service.d/50-apex-bootcount.conf'
	$(INSTALL) -Dm0644 $(BOOT_DATA)/rescue.service.d/50-apex-bootcount.conf \
		'$(1)$(UNITDIR)/rescue.service.d/50-apex-bootcount.conf'
	$(INSTALL) -Dm0644 $(AGENT_DATA)/apex-session-agent.service \
		'$(1)$(USERUNITDIR)/apex-session-agent.service'
	$(INSTALL) -Dm0644 $(AGENT_DATA)/80-apex.preset \
		'$(1)$(USERPRESETDIR)/80-apex.preset'
endef

install-data:
	$(call install_data,$(DESTDIR))

# Validators missing on a dev host are skipped; the RPM %check and CI install all of them.
# The units name /usr paths: the staged root (stub binaries, no cargo build) needs PREFIX=/usr.
check-data:
	@if command -v xmllint >/dev/null 2>&1; then \
		echo "xmllint --noout --nonet $(XML_DATA)"; \
		xmllint --noout --nonet $(XML_DATA); \
	else \
		echo "skip: xmllint not found (Fedora: libxml2)"; \
	fi
	@set -e; \
	desktop=$$(find crates -path '*/data/*' -name '*.desktop' | sort); \
	metainfo=$$(find crates -path '*/data/*' -name '*.metainfo.xml' | sort); \
	if [ -z "$$desktop" ]; then \
		echo "skip: no desktop entries yet (the Updates app arrives in M2)"; \
	elif command -v desktop-file-validate >/dev/null 2>&1; then \
		echo "desktop-file-validate $$desktop"; desktop-file-validate $$desktop; \
	else \
		echo "skip: desktop-file-validate not found (Fedora: desktop-file-utils)"; \
	fi; \
	if [ -z "$$metainfo" ]; then \
		echo "skip: no metainfo files yet (M2)"; \
	elif command -v appstreamcli >/dev/null 2>&1; then \
		echo "appstreamcli validate --no-net $$metainfo"; \
		appstreamcli validate --no-net $$metainfo; \
	else \
		echo "skip: appstreamcli not found (Fedora: appstream)"; \
	fi
	@set -e; \
	rules=$$(find hardware -path '*/generated/*' -name '*.rules' | sort); \
	if [ -z "$$rules" ]; then \
		echo "skip: no generated udev rules"; \
	elif udevadm verify --help >/dev/null 2>&1; then \
		echo "udevadm verify $$rules"; udevadm verify $$rules; \
	else \
		echo "skip: udevadm verify not found (Fedora: systemd-udev)"; \
	fi
	@set -e; \
	if ! command -v systemd-analyze >/dev/null 2>&1; then \
		echo "skip: systemd-analyze not found (Fedora: systemd)"; \
		exit 0; \
	fi; \
	rm -rf '$(CHECK_ROOT)'; \
	$(MAKE) --no-print-directory -s install-data DESTDIR='$(CHECK_ROOT)'; \
	for f in '$(BINDIR)/apexctl' '$(LIBEXECDIR)/apex-updated' \
		'$(LIBEXECDIR)/apex-boot-assess' '$(LIBEXECDIR)/apex-session-agent' /usr/bin/true; do \
		mkdir -p "$$(dirname '$(CHECK_ROOT)'"$$f")"; \
		printf '#!/bin/sh\nexit 0\n' > '$(CHECK_ROOT)'"$$f"; \
		chmod 0755 '$(CHECK_ROOT)'"$$f"; \
	done; \
	for u in $(SYSTEMD_UNITS_WITH_APEX_DROPIN); do \
		printf '[Unit]\nDescription=stub of %s\nDefaultDependencies=no\n\n[Service]\nType=idle\nExecStart=/usr/bin/true\n' \
			"$$u" > '$(CHECK_ROOT)$(UNITDIR)/'"$$u"; \
	done; \
	echo "systemd-analyze verify --root=$(CHECK_ROOT) $(UNITS) $(SYSTEMD_UNITS_WITH_APEX_DROPIN)"; \
	systemd-analyze verify --man=no --recursive-errors=no \
		--root='$(CHECK_ROOT)' $(UNITS) $(SYSTEMD_UNITS_WITH_APEX_DROPIN); \
	user_unit='$(CHECK_ROOT)$(USERUNITDIR)/apex-session-agent.service'; \
	runtime=$$(mktemp -d); \
	trap 'rm -rf "$$runtime"' EXIT; \
	sed 's|^ExecStart=|ExecStart=$(CHECK_ROOT)|' "$$user_unit" >"$$runtime/apex-session-agent.service"; \
	echo "systemd-analyze --user verify apex-session-agent.service (ExecStart in the staged root)"; \
	XDG_RUNTIME_DIR="$$runtime" systemd-analyze --user verify --man=no --recursive-errors=no \
		"$$runtime/apex-session-agent.service"

rpm:
	@if [ -z '$(SHORTCOMMIT)' ]; then \
		echo "error: 'make rpm' packages git HEAD, and this checkout has no commit" >&2; \
		exit 1; \
	fi
	@if [ '$(SPEC_VERSION)' != '$(CRATE_VERSION)' ]; then \
		echo "error: $(SPEC) Version '$(SPEC_VERSION)' differs from the workspace" \
			"version '$(CRATE_VERSION)' in Cargo.toml" >&2; \
		exit 1; \
	fi
	@git diff --quiet HEAD -- 2>/dev/null || \
		echo "warning: the RPM is built from HEAD; uncommitted changes are not included" >&2
	mkdir -p '$(RPM_TOPDIR)/SOURCES'
	git archive --format=tar.gz --prefix='apex-$(SPEC_VERSION)/' \
		-o '$(RPM_TOPDIR)/SOURCES/apex-$(SPEC_VERSION).tar.gz' HEAD
	$(RPMBUILD) -ba $(SPEC) \
		--define '_topdir $(RPM_TOPDIR)' \
		--define 'commitdate $(COMMITDATE)' \
		--define 'shortcommit $(SHORTCOMMIT)' \
		$(RPMBUILD_FLAGS)

clean:
	$(CARGO) clean
	rm -rf '$(RPM_TOPDIR)' '$(abspath $(CARGO_TARGET_DIR))/check-data'
