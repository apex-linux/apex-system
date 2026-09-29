#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

usage() {
    cat >&2 <<'EOF'

Content keys and build of the apex and apex-desktop RPMs, inside
registry.fedoraproject.org/fedora:45 as root.

  rpms.sh key                  print the content key of the apex RPM for git HEAD
  rpms.sh build OUTDIR [KEY]   install build deps, `make rpm` and `make rpm-desktop`,
                               copy the binary RPMs to OUTDIR with their keys (KEY,
                               KEY_DESKTOP); with KEY, fail if the apex key changed
                               meanwhile
  rpms.sh toolchain            install Fedora's rust, cargo, clippy, rustfmt and gcc
                               from the image compose's repositories
EOF
    exit 2
}

# Bump a schema when the inputs or the format of its key change.
readonly KEY_SCHEMA=3
# LICENSE and README.md are the RPM's %license and %doc.
readonly KEY_PATHS=(crates packaging/apex.spec Makefile Cargo.lock Cargo.toml LICENSE README.md)
readonly DESKTOP_KEY_SCHEMA=1
readonly DESKTOP_KEY_PATHS=(desktop packaging/apex-desktop.spec Makefile LICENSE)
readonly RECIPE_PATHS=(.github/scripts/rpms.sh)
readonly SPEC="$APEX_ROOT/packaging/apex.spec"
readonly DESKTOP_SPEC="$APEX_ROOT/packaging/apex-desktop.spec"
readonly INSTALL_TOOL="$APEX_ROOT/.github/scripts/install-tool.sh"

git_() { git -c safe.directory="$APEX_ROOT" -C "$APEX_ROOT" "$@"; }

COMPOSE_REPOS=()
load_compose_repos() { mapfile -t COMPOSE_REPOS < <(dnf_repo_args); }

repo_rust_nevra() {
    local nevra
    load_compose_repos
    nevra=$(retry 3 10 dnf5 repoquery --quiet "${COMPOSE_REPOS[@]}" --latest-limit=1 --arch="$(uname -m)" \
        --queryformat '%{name}-%{evr}.%{arch}\n' rust | tail -n 1)
    [[ $nevra == rust-* ]] || die "could not resolve the rust package in the Fedora repositories"
    printf '%s\n' "$nevra"
}

# `make rpm` passes no --with/--without, so the spec's default decides.
spec_auditable() {
    if grep -Eq '^[[:space:]]*%bcond[[:space:]]+auditable[[:space:]]+1([[:space:]]|$)|^[[:space:]]*%bcond_without[[:space:]]+auditable([[:space:]]|$)' "$1"; then
        printf 'on\n'
    else
        printf 'off\n'
    fi
}

auditable_pin() {
    if [[ $(spec_auditable "$SPEC") == on ]]; then
        "$INSTALL_TOOL" --pin cargo-auditable
    else
        printf 'off\n'
    fi
}

key_of() {
    {
        printf 'apex-rpms key schema %s\n' "$KEY_SCHEMA"
        printf 'rust %s\n' "$1"
        printf 'cargo-auditable %s\n' "$2"
        printf 'recipe %s\n' "$3"
        printf '%s\n' "$4"
    } | sha256sum | cut -c1-64
}

recipe() {
    local p
    printf 'repos %s\n' "$(dnf_repo_args | paste -sd' ' -)"
    for p in "${RECIPE_PATHS[@]}"; do
        git_ cat-file -e "HEAD:$p" 2>/dev/null ||
            die "$p is not committed at HEAD; the RPM key covers committed files only"
    done
    git_ ls-tree HEAD -- "${RECIPE_PATHS[@]}"
}

# The RPM never carries apex-lane, but Cargo unifies features across the workspace, so its
# Cargo.toml stays in.
apex_tree() {
    git_ ls-tree -r HEAD -- "${KEY_PATHS[@]}" |
        awk -F '\t' '$2 !~ /^crates\/apex-lane\// || $2 == "crates/apex-lane/Cargo.toml"'
}

compute_key() {
    local rust=$1 tree p pin rec
    for p in "${KEY_PATHS[@]:1}"; do
        git_ cat-file -e "HEAD:$p" 2>/dev/null ||
            die "$p is not committed at HEAD; the RPM key covers committed files only"
    done
    # One assignment each, so a failing $(...) stops the script instead of shortening the key's input.
    tree=$(apex_tree)
    pin=$(auditable_pin)
    rec=$(recipe)
    key_of "$rust" "$pin" "$rec" "$tree"
}

desktop_key_of() {
    {
        printf 'apex-desktop key schema %s\n' "$DESKTOP_KEY_SCHEMA"
        printf 'recipe %s\n' "$1"
        printf '%s\n' "$2"
    } | sha256sum | cut -c1-64
}

# desktop/shell/ ships inside the rebuilt gnome-shell, which apex-lane keys.
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
    load_compose_repos
    log "installing Fedora's rust toolchain from: ${COMPOSE_REPOS[*]}"
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${COMPOSE_REPOS[@]}" install \
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
    load_compose_repos
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${COMPOSE_REPOS[@]}" install \
        dnf5-plugins rpm-build make git-core findutils tar xz curl python3
    retry 3 10 dnf5 -y --setopt=install_weak_deps=False "${COMPOSE_REPOS[@]}" builddep "$SPEC" "$DESKTOP_SPEC"
    if [[ $(spec_auditable "$SPEC") == on ]]; then
        # Fedora does not package cargo-auditable, so the spec cannot BuildRequire it.
        "$INSTALL_TOOL" cargo-auditable /usr/local/bin
        # No grep -q: it could stop reading early and make cargo fail with SIGPIPE.
        cargo --list | grep -E '^[[:space:]]+auditable([[:space:]]|$)' >/dev/null ||
            die "cargo does not list the auditable subcommand after installing cargo-auditable"
    else
        log "apex.spec builds without cargo-auditable by default; not installing it"
    fi
    endgroup
}

# The Makefile runs git, and a checkout owned by another uid (a bind mount into a rootful
# container) needs safe.directory; this sets it for the one command, in no gitconfig file.
make_in_checkout() {
    GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$APEX_ROOT" \
        make -C "$APEX_ROOT" "$@"
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
    # Failing on a rust that rust-toolchain.toml does not follow yet would hold back every
    # OS security update until the pin moves.
    "$APEX_ROOT/.github/scripts/rust-toolchain-check.sh" --warn-only

    group "License tag"
    python3 "$APEX_ROOT/packaging/licenses.py" --check "$SPEC"
    endgroup

    group "make rpm"
    # A reused checkout must never ship an RPM left over from an earlier build.
    rm -rf "$APEX_ROOT/target/rpm"
    make_in_checkout rpm
    endgroup

    group "make rpm-desktop"
    make_in_checkout rpm-desktop
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

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
