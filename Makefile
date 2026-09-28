# SPDX-License-Identifier: GPL-3.0-or-later
#
# GNU make. packaging/apex.spec's %install runs `make install`, so this file alone
# defines the installed layout.
#
#   make build                   release binaries (cargo auditable when it is installed)
#   make install DESTDIR=DIR     install the layout; never builds
#   make check-data              validate D-Bus, polkit, desktop and systemd data files
#   make rpm                     SRPM and RPMs from git HEAD with rpmbuild (packaging/README.md)
#   make rpm-desktop             the apex-desktop SRPM and RPM from git HEAD (desktop/, without
#                                desktop/shell/, which ships in the rebuilt gnome-shell)
#   make clean

PREFIX      ?= /usr
BINDIR      ?= $(PREFIX)/bin
LIBEXECDIR  ?= $(PREFIX)/libexec/apex
DATADIR     ?= $(PREFIX)/share
UNITDIR     ?= $(PREFIX)/lib/systemd/system
TMPFILESDIR ?= $(PREFIX)/lib/tmpfiles.d
DBUSDIR     ?= $(DATADIR)/dbus-1
POLKITDIR   ?= $(DATADIR)/polkit-1/actions
BOOTUPDDIR  ?= $(PREFIX)/lib/bootupd/grub2-static/configs.d
DESTDIR     ?=

CARGO       ?= cargo
INSTALL     ?= install
RPMBUILD    ?= rpmbuild

# auto: use `cargo auditable` when it is installed, else plain cargo with a warning.
# yes:  fail when it is missing (the RPM build passes this, see packaging/apex.spec).
# no:   never use it.
AUDITABLE        ?= auto
CARGO_BUILD_ARGS ?= --release --locked
# Cargo honours CARGO_TARGET_DIR too, so both agree on where the binaries land.
CARGO_TARGET_DIR ?= target
RELEASE_DIR      := $(CARGO_TARGET_DIR)/release

# Selected with --bin, not --workspace or --bins, to keep GUI crates and their GTK
# dependency out of this build.
APEX_BINS := apexctl apex-updated apex-boot-assess

DBUS_IFACE_XML := crates/apex-dbus/data/io.github.sekirokenjii.Apex.Update1.xml
UPDATED_DATA   := crates/apex-updated/data
BOOT_DATA      := crates/apex-boot/data

XML_DATA := $(DBUS_IFACE_XML) \
	$(UPDATED_DATA)/io.github.sekirokenjii.Apex.Update1.conf \
	$(UPDATED_DATA)/io.github.sekirokenjii.apex.update.policy
UNITS := apex-updated.service apex-update-auto.service apex-update-auto.timer \
	apex-boot-assess.service
# systemd's own units that get an Apex drop-in. check-data verifies them with a stub of
# the unit itself, since the check root holds only Apex files.
DROPIN_UNITS := emergency.service rescue.service
CHECK_ROOT := $(abspath $(CARGO_TARGET_DIR))/check-data/root

# RPM build from git HEAD. These are recursively expanded (=) on purpose, so git runs
# only when `make rpm` actually needs the values.
SPEC          := packaging/apex.spec
RPM_TOPDIR    := $(abspath $(CARGO_TARGET_DIR))/rpm
SPEC_VERSION   = $(shell sed -n 's/^Version:[[:space:]]*//p' $(SPEC))
CRATE_VERSION  = $(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml)
COMMITDATE     = $(shell TZ=UTC git log -1 --format=%cd --date=format-local:%Y%m%d HEAD 2>/dev/null)
SHORTCOMMIT    = $(shell git rev-parse --short=12 HEAD 2>/dev/null)
RPMBUILD_FLAGS ?=
DESKTOP_SPEC     := packaging/apex-desktop.spec
DESKTOP_VERSION   = $(shell sed -n 's/^Version:[[:space:]]*//p' $(DESKTOP_SPEC))

.PHONY: all build install install-bin install-data check-data rpm rpm-desktop clean help
.DELETE_ON_ERROR:

all: build

help:
	@echo 'Targets: build install check-data rpm rpm-desktop clean'
	@echo 'Common variables: DESTDIR PREFIX LIBEXECDIR CARGO AUDITABLE=auto|yes|no RPMBUILD_FLAGS'

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

# $(call install_data,ROOT): the data files, below ROOT.
define install_data
	$(INSTALL) -Dm0644 $(DBUS_IFACE_XML) \
		'$(1)$(DBUSDIR)/interfaces/io.github.sekirokenjii.Apex.Update1.xml'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-updated.service \
		'$(1)$(UNITDIR)/apex-updated.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-update-auto.service \
		'$(1)$(UNITDIR)/apex-update-auto.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/apex-update-auto.timer \
		'$(1)$(UNITDIR)/apex-update-auto.timer'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.sekirokenjii.Apex.Update1.service \
		'$(1)$(DBUSDIR)/system-services/io.github.sekirokenjii.Apex.Update1.service'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.sekirokenjii.Apex.Update1.conf \
		'$(1)$(DBUSDIR)/system.d/io.github.sekirokenjii.Apex.Update1.conf'
	$(INSTALL) -Dm0644 $(UPDATED_DATA)/io.github.sekirokenjii.apex.update.policy \
		'$(1)$(POLKITDIR)/io.github.sekirokenjii.apex.update.policy'
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
endef

install-data:
	$(call install_data,$(DESTDIR))

# Each tool is optional on a development host; a missing one is reported and skipped.
# The RPM %check and CI install all of them. systemd-analyze verifies the units inside
# a staged root that holds the data files plus stub binaries (and stubs of the systemd
# units that get Apex drop-ins), so the check needs no cargo build and never looks at
# the host's /usr. The units name /usr paths, so this check assumes the default PREFIX.
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
	if ! command -v systemd-analyze >/dev/null 2>&1; then \
		echo "skip: systemd-analyze not found (Fedora: systemd)"; \
		exit 0; \
	fi; \
	rm -rf '$(CHECK_ROOT)'; \
	$(MAKE) --no-print-directory -s install-data DESTDIR='$(CHECK_ROOT)'; \
	for f in '$(BINDIR)/apexctl' '$(LIBEXECDIR)/apex-updated' \
		'$(LIBEXECDIR)/apex-boot-assess' /usr/bin/true; do \
		mkdir -p "$$(dirname '$(CHECK_ROOT)'"$$f")"; \
		printf '#!/bin/sh\nexit 0\n' > '$(CHECK_ROOT)'"$$f"; \
		chmod 0755 '$(CHECK_ROOT)'"$$f"; \
	done; \
	for u in $(DROPIN_UNITS); do \
		printf '[Unit]\nDescription=stub of %s\nDefaultDependencies=no\n\n[Service]\nType=idle\nExecStart=/usr/bin/true\n' \
			"$$u" > '$(CHECK_ROOT)$(UNITDIR)/'"$$u"; \
	done; \
	echo "systemd-analyze verify --root=$(CHECK_ROOT) $(UNITS) $(DROPIN_UNITS)"; \
	systemd-analyze verify --man=no --recursive-errors=no \
		--root='$(CHECK_ROOT)' $(UNITS) $(DROPIN_UNITS)

# Packages the committed tree (git archive HEAD): uncommitted changes are not in the RPM.
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

# Packages the committed tree, like rpm.
rpm-desktop:
	@if [ -z '$(SHORTCOMMIT)' ]; then \
		echo "error: 'make rpm-desktop' packages git HEAD, and this checkout has no commit" >&2; \
		exit 1; \
	fi
	@git diff --quiet HEAD -- desktop $(DESKTOP_SPEC) 2>/dev/null || \
		echo "warning: the RPM is built from HEAD; uncommitted changes are not included" >&2
	mkdir -p '$(RPM_TOPDIR)/SOURCES'
	git archive --format=tar.gz --prefix='apex-desktop-$(DESKTOP_VERSION)/' \
		-o '$(RPM_TOPDIR)/SOURCES/apex-desktop-$(DESKTOP_VERSION).tar.gz' HEAD \
		-- desktop ':(exclude)desktop/shell' LICENSE
	$(RPMBUILD) -ba $(DESKTOP_SPEC) \
		--define '_topdir $(RPM_TOPDIR)' \
		--define 'commitdate $(COMMITDATE)' \
		--define 'shortcommit $(SHORTCOMMIT)' \
		$(RPMBUILD_FLAGS)

clean:
	$(CARGO) clean
	rm -rf '$(RPM_TOPDIR)' '$(abspath $(CARGO_TARGET_DIR))/check-data'
