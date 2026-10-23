# SPDX-License-Identifier: GPL-3.0-or-later
#
# Recipes never run sudo: the ones that need root print the command instead.

set shell := ["bash", "-euo", "pipefail", "-c"]

profile := "hardware/m7400qc/profile.toml"
generated := "hardware/m7400qc/generated"

# List the recipes.
default:
    @just --list

# fmt --check, clippy with warnings denied, the tests and the comment guard, as ci.yml runs them.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    cargo run -p apex-lint --locked -- comments

# The comment guard (docs/CONTRIBUTING.md) on every tracked file, or on the given files and directories.
[positional-arguments]
lint-comments *paths:
    cargo run -q -p apex-lint --locked -- comments "$@"

# Format the workspace.
fmt:
    cargo fmt --all

# Advisories, licenses, bans and sources (deny.toml); needs cargo-deny.
deny:
    cargo deny --locked check

# ci.yml's privacy guard on tracked and untracked files: run it before `git add` after fingerprint work.
privacy:
    .github/scripts/privacy-guard.sh --untracked

# The image checks' tests, also run by ci.yml (files needing bwrap or systemd tools skip here without them).
image-test:
    image/tests/run.sh

# The Apex layer's safety net in gjs, also run by ci.yml: kill switches, fault injection, crash guard.
layer-test:
    tests/layer/run.sh

# The dev loop's host side against a fake ssh, also run by ci.yml: the throwaway guard, desktop-update, shell-dev fetch.
devloop-test:
    tests/devloop/run.sh

# G2's own logic without the VM, also run by ci.yml: the capture comparison, the token file, the top-bar assertions.
shots-test:
    tests/shots/selftest.sh

# Render hardware/m7400qc/generated from the hardware profile.
render:
    cargo run --locked -p apexctl -- profile render --profile {{ profile }} --out {{ generated }}

# Exit 1 with a diff when the committed renders differ from the profile.
render-check:
    cargo run --locked -p apexctl -- profile render --profile {{ profile }} --out {{ generated }} --check

# Build the apex RPM from git HEAD (Fedora only: the builder VM or a fedora:45 toolbox).
rpm:
    #!/usr/bin/env bash
    set -euo pipefail
    id=$(. /etc/os-release && printf '%s' "${ID:-}")
    if [[ $id != fedora ]]; then
        echo "'make rpm' needs Fedora 45 (rpmbuild, Fedora's rust and cargo-rpm-macros)." >&2
        echo "Run it in the builder VM, or: toolbox create -i registry.fedoraproject.org/fedora-toolbox:45" >&2
        echo "The builder VM builds it with: .github/scripts/rpms.sh build OUTDIR (as root in fedora:45)." >&2
        exit 1
    fi
    make rpm

# How to build the image (it runs in the Fedora 45 builder VM, not on this host).
image:
    @echo 'The image builds with rootful podman in the Fedora 45 KVM builder VM (ADR 0013): just vm-create, then just vm-image.'
    @echo 'In the VM, from a checkout of this repository:'
    @echo ''
    @echo '  sudo dnf install podman skopeo jq git-core rpm-sign gnupg2'
    @echo '  # RPM inputs: build them'
    @echo '  sudo podman run --rm -v "$PWD:/src:z" -w /src registry.fedoraproject.org/fedora:45 .github/scripts/rpms.sh build /src/_build/rpms'
    @echo '  sudo podman run --rm -v "$PWD:/src:z" -w /src registry.fedoraproject.org/fedora:45 .github/scripts/libfprint.sh build /src/_build/libfprint   # exit 3 = degraded'
    @echo '  # sign both with the Apex RPM key, as build-image.sh does: [apex] has gpgcheck=1 (packaging/README.md, Signing)'
    @echo '  sudo chown -R "$(id -u):$(id -g)" _build/rpms _build/libfprint'
    @echo '  for d in _build/rpms _build/libfprint; do tools/builder-vm/lane.sh sign "$d"/*.rpm && (cd "$d" && sha256sum -- *.rpm >SHA256SUMS); done'
    @echo '  tools/builder-vm/lane.sh build all  # the rebuild lane, apex-lane (gnome-shell); exit 3 = held, degraded or stock'
    @echo '  .github/scripts/image.sh context --rpms-dir _build/rpms --libfprint-dir _build/libfprint --lane-lock ~/apex-store/rpms.lock.next'
    @echo '  .github/scripts/image.sh build'
    @echo '  .github/scripts/image.sh inspect'
    @echo '  tests/shots/run.sh                  # G2, the headless shell, twice (tests/shots/README.md)'
    @echo '  .github/scripts/qualify.sh g4-key localhost/apex:build >_build/image/g4-key.txt'
    @echo '  .github/scripts/qualify.sh g4-due _build/image/g4-key.txt && .github/scripts/qualify.sh local localhost/apex:build   # G4 when due'
    @echo '  .github/scripts/image.sh rechunk      # optional locally; output in _build/image/oci'
    @echo ''
    @echo 'Nothing publishes yet: `just publish` (push, sign, tag) comes in roadmap S1d. GitHub runs only ci.yml.'
    @echo 'Faster loop for system_files-only changes: FROM localhost/apex:build (the last image.sh build) plus COPY, tested with bcvk ephemeral.'

# Print the sudo command that dumps the ALC294 COEFs for PHASE (A-E, see coef-dump.sh).
audio-dump phase="A":
    #!/usr/bin/env bash
    set -euo pipefail
    case '{{ phase }}' in
        A|B|C|D|E) ;;
        *) echo "PHASE must be one of A B C D E (see: hardware/m7400qc/audio/coef-dump.sh --help)" >&2; exit 1 ;;
    esac
    echo "Run this yourself (it reads the codec through hda-verb, so it needs root):"
    echo
    echo "  sudo $PWD/hardware/m7400qc/audio/coef-dump.sh {{ phase }}"
    echo
    echo "Then paste its TOML fragment into {{ profile }} ([audio.measurements]) and run: just render"

# How to run the KVM qualification locally (in the builder VM, with nested KVM).
qualify-local:
    @echo 'Qualification runs .github/scripts/qualify.sh in the builder VM; GitHub does not run it:'
    @echo ''
    @echo '  sudo dnf install qemu-system-x86 edk2-ovmf dosfstools btrfs-progs openssh-clients jq skopeo podman openssl'
    @echo '  test -w /dev/kvm   # nested KVM must be on (kvm_amd nested=1 on the host)'
    @echo '  .github/scripts/qualify.sh fresh ghcr.io/sekirokenjii/apex-testing@sha256:<digest>'
    @echo '  .github/scripts/qualify.sh all   ghcr.io/sekirokenjii/apex-testing@sha256:<digest>'
    @echo ''
    @echo 'The counted rollback on its own (FROM boots first; CANDIDATE is forced red through'
    @echo '/var/lib/apex/force-red, must reboot itself 3 times, and GRUB must fall back to FROM):'
    @echo ''
    @echo '  .github/scripts/qualify.sh red sha256:<from digest> sha256:<candidate digest>'
    @echo ''
    @echo 'It uses sudo for podman, losetup and mount, writes to _build/qualify (fixtures/, logs/,'
    @echo 'results.md) and needs about 60 GiB free. QUALIFY_LAYOUT=to-disk switches to the'
    @echo 'bootc install to-disk --via-loopback path.'

# ---- Fedora 45 builder VM (tools/builder-vm/README.md) --------------------------------

# Create the builder VM: download and verify Fedora Cloud 45, first boot, provision.
vm-create:
    tools/builder-vm/builder-vm.sh create

# Boot the builder VM in the background.
vm-start:
    tools/builder-vm/builder-vm.sh start

# Shut the builder VM down cleanly.
vm-stop:
    tools/builder-vm/builder-vm.sh stop

# Show whether the builder VM runs, and its disk and memory use.
vm-status:
    tools/builder-vm/builder-vm.sh status

# A shell in the builder VM (or: just vm-ssh 'CMD', run by the VM's shell).
[positional-arguments]
vm-ssh *args:
    tools/builder-vm/builder-vm.sh ssh "$@"

# Copy this checkout into the builder VM (~/apex-fedora).
vm-sync:
    tools/builder-vm/builder-vm.sh sync

# Build the RPMs, libfprint, the rebuild lane and the image in the builder VM (nothing is pushed), then G2 twice, and G4 when qualify.sh g4-due says it is due.
vm-image:
    tools/builder-vm/builder-vm.sh image

# Rebuild only the image in the builder VM, reusing the RPMs and the lane lock of the last vm-image.
vm-image-only:
    tools/builder-vm/builder-vm.sh image --image-only

# G4 on the last built image in the builder VM: install, counted rollback, GDM, login, lock and unlock (tools/builder-vm/README.md).
vm-qualify:
    tools/builder-vm/builder-vm.sh run env QUALIFY_MEM=3G QUALIFY_SMP=4 .github/scripts/qualify.sh local localhost/apex:build

# G2 in the builder VM: the headless shell of IMAGE (the last vm-image build), run twice and asserted (tests/shots/README.md).
vm-g2 image="localhost/apex:build":
    tools/builder-vm/builder-vm.sh run tests/shots/run.sh {{ image }}

# ---- Rebuild lane: Fedora packages with Apex patches (packaging/README.md) -------------
# snapshot.sh first commits a dirty tree in the VM copy: the lane and lane.sh build from git HEAD.

# Rebuild PKG (or all) from Fedora's SRPM with its series and Source1, in the builder VM.
pkg-build pkg="all":
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh build {{ pkg }}

# Print PKG's content key: Fedora's NVR plus a hash of the pin and every Apex input.
pkg-key pkg:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh key {{ pkg }}

# What the next image takes from the lane: each family's state, build and hold timer.
pkg-status:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh status

# A git work tree of PKG in the builder VM: Fedora's %prep, the series as commits, Source1.
pkg-prep pkg:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh prep {{ pkg }}

# The series onto another Fedora build of PKG (git am -3) in the work tree; then pkg-export.
pkg-rebase pkg nvr:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh prep {{ pkg }} {{ nvr }}

# Write the work tree's commits to packaging/pkgs/PKG (patches/, series) on this host.
pkg-export pkg *new:
    tools/builder-vm/builder-vm.sh sync
    tools/builder-vm/builder-vm.sh ssh 'cd ~/apex-fedora && tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh export {{ pkg }} {{ new }} >&2 && tar -C packaging/pkgs/{{ pkg }} -cf - series patches' | tar -C packaging/pkgs/{{ pkg }} -xf -

# After an image built from rpms.lock.next passed its gates, make it rpms.lock.
pkg-promote:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh promote

# At a hold limit, ship Fedora's build of PKG's family instead (an owner decision).
pkg-stock pkg:
    tools/builder-vm/builder-vm.sh run tools/builder-vm/snapshot.sh tools/builder-vm/lane.sh stock {{ pkg }}

# ---- Desktop VM: the built image in a window on this host (tools/builder-vm/README.md) ---

# Install the last vm-image build onto the desktop VM disk (desktop-install --replace redoes it).
[positional-arguments]
desktop-install *args:
    tools/builder-vm/desktop-vm.sh install "$@"

# Open the desktop VM window (APEX_DESKTOP_SHARP=1 just desktop: 1:1 pixels on a scaled host).
desktop:
    tools/builder-vm/desktop-vm.sh start

# Shut the desktop VM down cleanly.
desktop-stop:
    tools/builder-vm/desktop-vm.sh stop

# Show whether the desktop VM runs and which image it has.
desktop-status:
    tools/builder-vm/desktop-vm.sh status

# A root shell in the desktop VM (or: just desktop-ssh 'CMD', run by the guest's shell).
[positional-arguments]
desktop-ssh *args:
    tools/builder-vm/desktop-vm.sh ssh "$@"

# Stage the last vm-image build in the running desktop VM (podman save | load, bootc switch); keeps /var/home and /etc.
[positional-arguments]
desktop-update *args:
    tools/builder-vm/desktop-vm.sh update "$@"

# ---- Dev loop: the shell in the desktop VM (desktop/README.md) --------------------------

# JS and SCSS edits in a nested devkit shell in the desktop VM within seconds (first: just shell-dev fetch).
[positional-arguments]
shell-dev *args:
    tools/builder-vm/shell-dev.sh "$@"

# How to rebuild gnome-shell's C incrementally in an F45 toolbox in the desktop VM and run it through bootc usroverlay.
shell-c:
    @echo 'The C loop runs in the desktop VM: gnome-shell from the Apex SRPM of the build the VM runs, compiled'
    @echo 'with ninja in an F45 toolbox, then unpacked over a transient /usr (desktop/README.md, "The C loop").'
    @echo 'On this host:'
    @echo ''
    @echo '  just shell-dev fetch && just shell-dev push     # the SRPM lands in /var/lib/apex-shell-dev/inputs/rpms/'
    @echo ''
    @echo 'In a terminal in the VM window, as your user, once per shell build (about 15 minutes the first time, mostly downloads):'
    @echo ''
    @echo '  mkdir -p ~/apex-shell-c && cp /var/lib/apex-shell-dev/inputs/rpms/gnome-shell-*.src.rpm ~/apex-shell-c/'
    @echo '  srpm=$(ls ~/apex-shell-c/gnome-shell-*.src.rpm)    # the toolbox sees your home, not /var/lib'
    @echo '  toolbox create --distro fedora --release 45 apex-shell'
    @echo '  toolbox run -c apex-shell sudo dnf -y install rpm-build'
    @echo '  toolbox run -c apex-shell sudo dnf -y builddep "$srpm"'
    @echo '  toolbox run -c apex-shell rpmbuild -rc --noclean --define "_topdir $HOME/apex-shell-c" --define "_package_note_status 0" "$srpm"'
    @echo '  cd ~/apex-shell-c/BUILD/gnome-shell-*-build/gnome-shell-*/    # the tree: edit src/ here'
    @echo ''
    @echo 'Each round, from the tree (seconds to build and install):'
    @echo ''
    @echo '  toolbox run -c apex-shell ninja -C redhat-linux-build'
    @echo '  toolbox run -c apex-shell meson install -C redhat-linux-build --no-rebuild --destdir "$HOME/apex-shell-c/root"'
    @echo '  sudo bootc usroverlay    # once per boot: /usr stays writable until the next reboot, which drops the build'
    @echo '  tar -C ~/apex-shell-c/root -cf - usr | sudo tar -C / --no-same-owner --no-overwrite-dir -xf -'
    @echo ''
    @echo 'Then just shell-dev on this host (the nested shell), or log out and in (the session shell), runs it.'
