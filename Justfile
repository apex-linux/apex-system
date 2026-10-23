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
    apex-lint comments

# The comment guard on every tracked file, or on the given ones (cargo install --git https://github.com/apex-linux/apex-ci apex-lint).
[positional-arguments]
lint-comments *paths:
    apex-lint comments "$@"

# Format the workspace.
fmt:
    cargo fmt --all

# Advisories, licenses, bans and sources (deny.toml); needs cargo-deny.
deny:
    cargo deny --locked check

# The script self-tests, as ci.yml runs them.
selftest:
    .github/scripts/selftest.sh

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
