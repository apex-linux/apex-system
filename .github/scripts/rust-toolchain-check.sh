#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Assert that the rustc on PATH is the release pinned in rust-toolchain.toml.
#
#   rust-toolchain-check.sh              fail on a mismatch (rust.yml)
#   rust-toolchain-check.sh --warn-only  report a mismatch as a warning (rpms.sh build:
#                                        the RPM is built with Fedora's current rust
#                                        anyway, and OS updates must not stop for it)
#
# CI runs this in registry.fedoraproject.org/fedora:45 with Fedora's rust and cargo.
# Fedora's /usr/bin/cargo is the real cargo, not a rustup proxy (rust.spec, %files -n
# cargo; the Fedora rustup package ships only rustup-init), so rust-toolchain.toml is
# inert there: nothing tries to download a toolchain. The pin exists so that developer machines with
# rustup build with the same compiler that builds the RPM, and this check keeps the two
# in step (ARCHITECTURE section 11).

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

warn_only=0
case ${1:-} in
    "") ;;
    --warn-only) warn_only=1 ;;
    *) usage ;;
esac

require rustc sed

toolchain_file="$APEX_ROOT/rust-toolchain.toml"
[[ -f $toolchain_file ]] || die "missing $toolchain_file"

pinned=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$toolchain_file")
[[ -n $pinned ]] || die "no channel = \"...\" line in rust-toolchain.toml"

actual=$(rustc -vV | sed -n 's/^release: //p')
[[ -n $actual ]] || die "could not read the release from 'rustc -vV'"

if command -v rustup >/dev/null 2>&1 && [[ -z ${RUSTUP_TOOLCHAIN:-} ]]; then
    # With rustup, rustc is a proxy that honours rust-toolchain.toml; the comparison
    # below then checks the toolchain that rustup resolved.
    log "rustup found: rustc is a proxy and follows rust-toolchain.toml"
fi

log "rust-toolchain.toml channel: $pinned"
log "rustc on PATH ($(command -v rustc)): $actual"

if [[ $actual != "$pinned" ]]; then
    msg="rustc is $actual but rust-toolchain.toml pins $pinned. Fedora $APEX_FEDORA ships the \
compiler that builds the RPM; set channel = \"$actual\" in rust-toolchain.toml (and \
review rust-version in Cargo.toml) in the same change that moves to the new Fedora rust."
    if ((warn_only)); then
        warn "$msg"
        summary "Toolchain: rustc $actual differs from rust-toolchain.toml ($pinned); built anyway."
        exit 0
    fi
    die "$msg"
fi

if command -v rpm >/dev/null 2>&1; then
    summary "Toolchain: \`$(rpm -q rust cargo 2>/dev/null | tr '\n' ' ')\` matches rust-toolchain.toml ($pinned)."
fi
