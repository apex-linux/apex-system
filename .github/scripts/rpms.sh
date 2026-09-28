#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Content keys and build of the apex and apex-desktop RPMs (ADR 0013). The builder VM
# runs `build`; ci.yml runs `toolchain`.
#
#   rpms.sh key                  print the content key of the apex RPM for git HEAD
#   rpms.sh build OUTDIR [KEY]   install build deps, `make rpm` and `make rpm-desktop`,
#                                copy the binary RPMs to OUTDIR with their keys (KEY,
#                                KEY_DESKTOP); with KEY, fail if the apex key changed
#                                meanwhile
#   rpms.sh toolchain            install Fedora's rust, cargo, clippy, rustfmt and gcc
#                                from the same repositories (ci.yml)
#
# Run them inside registry.fedoraproject.org/fedora:45 as root (ci.yml does, and so
# does the builder VM with `podman run --rm -v "$PWD:/src:Z" -w /src
# registry.fedoraproject.org/fedora:45 ...`).
#
# Repositories: every dnf5 call passes lib.sh's dnf_repo_args, the image compose's set
# (fedora, updates, and only the allowlisted updates-testing builds before GA). The
# pre-GA fedora:45 image enables all of updates-testing, which must never reach the
# compiler or the macros that build the RPM.
#
# The key is the SHA-256 of:
#   - a schema line (bump KEY_SCHEMA when the key's format changes),
#   - the NEVRA of the rust package those repositories currently resolve,
#   - the cargo-auditable pin of install-tool.sh ("<version> <sha256>"), or "off" when
#     the spec's `auditable` bcond defaults to off: the binaries embed its output,
#   - the recipe: the dnf5 repository options above and `git ls-tree HEAD` of this
#     script, so a change to how the RPM is built never reuses RPMs built the old way,
#   - `git ls-tree -r HEAD crates packaging/apex.spec Makefile Cargo.lock Cargo.toml
#     LICENSE README.md` (blob hashes of every committed input of the RPM; the last two
#     are packaged as %license and %doc).
# It is also the tag of ghcr.io/sekirokenjii/apex-rpms (lib.sh APEX_REPO_RPMS).
#
# apex-desktop (noarch data, packaging/apex-desktop.spec) has its own key, so an asset
# change never changes the apex key: a schema line, the same recipe, and `git ls-tree -r
# HEAD desktop packaging/apex-desktop.spec Makefile LICENSE` without desktop/shell/, which
# ships inside the rebuilt gnome-shell (packaging/rebuild.sh keys it).
#
# The build also runs `packaging/licenses.py --check` first, so an RPM whose License tag
# no longer matches the linked crates is never built or published.

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

readonly KEY_SCHEMA=3
readonly KEY_PATHS=(crates packaging/apex.spec Makefile Cargo.lock Cargo.toml LICENSE README.md)
readonly DESKTOP_KEY_SCHEMA=1
readonly DESKTOP_KEY_PATHS=(desktop packaging/apex-desktop.spec Makefile LICENSE)
# The build recipe; the caller only runs `rpms.sh build` in fedora:45.
readonly RECIPE_PATHS=(.github/scripts/rpms.sh)
readonly SPEC="$APEX_ROOT/packaging/apex.spec"
readonly DESKTOP_SPEC="$APEX_ROOT/packaging/apex-desktop.spec"
readonly INSTALL_TOOL="$APEX_ROOT/.github/scripts/install-tool.sh"

git_() { git -c safe.directory="$APEX_ROOT" -C "$APEX_ROOT" "$@"; }

# The compose's repositories as a dnf5 option array (lib.sh dnf_repo_args).
REPOS=()
repo_args() { mapfile -t REPOS < <(dnf_repo_args); }

# The rust NEVRA that `dnf install rust` would pick in those repositories right now.
repo_rust_nevra() {
    local nevra
    repo_args
    nevra=$(retry 3 10 dnf5 repoquery --quiet "${REPOS[@]}" --latest-limit=1 --arch="$(uname -m)" \
        --queryformat '%{name}-%{evr}.%{arch}\n' rust | tail -n 1)
    [[ $nevra == rust-* ]] || die "could not resolve the rust package in the Fedora repositories"
    printf '%s\n' "$nevra"
}

# spec_auditable SPEC: "on" when SPEC builds with cargo auditable by default
# (`%bcond auditable 1`, or the older `%bcond_without auditable`), else "off". `make rpm`
# passes no --with/--without, so the default is what CI builds.
spec_auditable() {
    if grep -Eq '^[[:space:]]*%bcond[[:space:]]+auditable[[:space:]]+1([[:space:]]|$)|^[[:space:]]*%bcond_without[[:space:]]+auditable([[:space:]]|$)' "$1"; then
        printf 'on\n'
    else
        printf 'off\n'
    fi
}

# auditable_pin: what the key records for cargo-auditable.
auditable_pin() {
    if [[ $(spec_auditable "$SPEC") == on ]]; then
        "$INSTALL_TOOL" --pin cargo-auditable
    else
        printf 'off\n'
    fi
}

# key_of RUST AUDITABLE RECIPE TREE: the key over its inputs (see the header).
key_of() {
    {
        printf 'apex-rpms key schema %s\n' "$KEY_SCHEMA"
        printf 'rust %s\n' "$1"
        printf 'cargo-auditable %s\n' "$2"
        printf 'recipe %s\n' "$3"
        printf '%s\n' "$4"
    } | sha256sum | cut -c1-64
}

# recipe: how the RPM is built, as one line per input (repository options, then the
# blob of every RECIPE_PATHS file at HEAD).
recipe() {
    local p
    printf 'repos %s\n' "$(dnf_repo_args | paste -sd' ' -)"
    for p in "${RECIPE_PATHS[@]}"; do
        git_ cat-file -e "HEAD:$p" 2>/dev/null ||
            die "$p is not committed at HEAD; the RPM key covers committed files only"
    done
    git_ ls-tree HEAD -- "${RECIPE_PATHS[@]}"
}

compute_key() {
    local rust=$1 tree p pin rec
    for p in "${KEY_PATHS[@]:1}"; do
        git_ cat-file -e "HEAD:$p" 2>/dev/null ||
            die "$p is not committed at HEAD; the RPM key covers committed files only"
    done
    # Assigned one by one: a failure inside $(...) then stops the script instead of
    # silently shortening the key's input.
    tree=$(git_ ls-tree -r HEAD -- "${KEY_PATHS[@]}")
    pin=$(auditable_pin)
    rec=$(recipe)
    key_of "$rust" "$pin" "$rec" "$tree"
}

# desktop_key_of RECIPE TREE: the apex-desktop key over its inputs (see the header).
desktop_key_of() {
    {
        printf 'apex-desktop key schema %s\n' "$DESKTOP_KEY_SCHEMA"
        printf 'recipe %s\n' "$1"
        printf '%s\n' "$2"
    } | sha256sum | cut -c1-64
}

# desktop_tree: the committed apex-desktop inputs, without desktop/shell/.
desktop_tree() {
    local p
    for p in "${DESKTOP_KEY_PATHS[@]}"; do
        git_ cat-file -e "HEAD:$p" 2>/dev/null ||
            die "$p is not committed at HEAD; the apex-desktop key covers committed files only"
    done
    git_ ls-tree -r HEAD -- "${DESKTOP_KEY_PATHS[@]}" | awk -F '\t' '$2 !~ /^desktop\/shell\//'
}

compute_desktop_key() {
    local tree rec
    tree=$(desktop_tree)
    rec=$(recipe)
    desktop_key_of "$rec" "$tree"
}

cmd_toolchain() {
    (($# == 0)) || die "usage: rpms.sh toolchain"
    ((EUID == 0)) || die "rpms.sh toolchain installs packages; run it as root in a fedora:45 container"
    require dnf5
    repo_args
    log "installing Fedora's rust toolchain from: ${REPOS[*]}"
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${REPOS[@]}" install \
        rust cargo clippy rustfmt gcc
}

cmd_key() {
    require git dnf5 sha256sum
    local rust key
    rust=$(repo_rust_nevra)
    key=$(compute_key "$rust")
    log "rust: $rust"
    log "key:  $key"
    printf '%s\n' "$key"
}

install_deps() {
    group "Build dependencies ($(dnf_repo_args | paste -sd' ' -))"
    repo_args
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${REPOS[@]}" install \
        dnf5-plugins rpm-build make git-core findutils tar xz curl python3
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${REPOS[@]}" builddep "$SPEC" "$DESKTOP_SPEC"
    if [[ $(spec_auditable "$SPEC") == on ]]; then
        # Fedora does not package cargo-auditable, so the spec cannot BuildRequire it and
        # `make build AUDITABLE=yes` fails without it: the pinned release binary, SHA-256
        # checked, into /usr/local/bin (on PATH, so `cargo auditable` finds it).
        "$INSTALL_TOOL" cargo-auditable /usr/local/bin
        # No grep -q: it could stop reading early and make cargo fail with SIGPIPE.
        cargo --list | grep -E '^[[:space:]]+auditable([[:space:]]|$)' >/dev/null ||
            die "cargo does not list the auditable subcommand after installing cargo-auditable"
    else
        log "apex.spec builds without cargo-auditable by default; not installing it"
    fi
    endgroup
}

cmd_build() {
    (($# >= 1 && $# <= 2)) || die "usage: rpms.sh build OUTDIR [KEY]"
    local outdir=$1 expected=${2:-}
    ((EUID == 0)) || die "rpms.sh build installs packages; run it as root in a fedora:45 container"
    install_deps

    local installed key desktop_key
    installed=$(rpm -q --queryformat '%{name}-%{evr}.%{arch}\n' rust)
    key=$(compute_key "$installed")
    desktop_key=$(compute_desktop_key)
    if [[ -n $expected && $key != "$expected" ]]; then
        die "content key changed between resolve and build ($expected -> $key, rust now \
$installed); the Fedora repositories moved in between. Build again."
    fi
    # ci.yml tests with the rust-toolchain.toml release. A Fedora rust update that
    # rust-toolchain.toml does not follow yet is reported, not fatal: failing here would
    # hold back every OS security update until the pin moves.
    "$APEX_ROOT/.github/scripts/rust-toolchain-check.sh" --warn-only

    group "License tag"
    # Resolves Cargo.lock with Fedora's cargo (network, like the build itself).
    python3 "$APEX_ROOT/packaging/licenses.py" --check "$SPEC"
    endgroup

    group "make rpm"
    # CARGO_TARGET_DIR stays at its default (target/), so the RPMs land in target/rpm.
    # Start from an empty target/rpm so a reused checkout (the builder VM) never ships an
    # RPM left over from an earlier build.
    rm -rf "$APEX_ROOT/target/rpm"
    # The Makefile runs git (archive, log); a checkout owned by another uid (a bind mount
    # into a rootful container) needs safe.directory, given here as command-scope config
    # so no gitconfig file is touched.
    GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$APEX_ROOT" \
        make -C "$APEX_ROOT" rpm
    endgroup

    group "make rpm-desktop"
    # The same target/rpm, so the collection below takes both packages.
    GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$APEX_ROOT" \
        make -C "$APEX_ROOT" rpm-desktop
    endgroup

    mkdir -p "$outdir"
    local -a rpms
    mapfile -t rpms < <(find "$APEX_ROOT/target/rpm/RPMS" -type f -name '*.rpm' \
        ! -name '*-debuginfo-*' ! -name '*-debugsource-*' | sort)
    ((${#rpms[@]} > 0)) || die "make rpm produced no binary RPMs under target/rpm/RPMS"
    [[ " ${rpms[*]##*/} " == *' apex-desktop-'[0-9]* ]] ||
        die "make rpm-desktop produced no apex-desktop RPM under target/rpm/RPMS"
    cp -- "${rpms[@]}" "$outdir/"
    (cd "$outdir" && sha256sum -- *.rpm >SHA256SUMS)
    printf '%s\n' "$key" >"$outdir/KEY"
    printf '%s\n' "$desktop_key" >"$outdir/KEY_DESKTOP"

    summary "### apex RPMs" "" "Key \`$key\` (rust \`$installed\`, cargo-auditable \`$(auditable_pin)\`)," \
        "apex-desktop key \`$desktop_key\`" "" '```' "$(cd "$outdir" && cat SHA256SUMS)" '```'
    log "built ${#rpms[@]} RPM(s) into $outdir"
}

main() {
    local cmd=${1:-}
    shift || true
    case $cmd in
        key) cmd_key "$@" ;;
        build) cmd_build "$@" ;;
        toolchain) cmd_toolchain "$@" ;;
        *) usage ;;
    esac
}

# selftest.sh sources this file to test its functions; only a direct run starts main.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
