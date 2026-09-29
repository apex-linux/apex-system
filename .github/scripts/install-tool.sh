#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

usage() {
    cat >&2 <<'EOF'

Install a pinned, prebuilt release binary after checking its SHA-256.

  install-tool.sh TOOL DESTDIR   download, verify and install DESTDIR/TOOL
  install-tool.sh --pin TOOL     print TOOL's pin, "<version> <sha256>" (no download)

TOOL is one of: cargo-deny, cargo-auditable, cosign, actionlint, shellcheck.
EOF
    exit 2
}

# Pinned here, not by third-party installer actions: secret-bearing jobs run first-party actions only.
# renovate: datasource=github-release-attachments depName=EmbarkStudios/cargo-deny
CARGO_DENY_VERSION=0.20.2
CARGO_DENY_SHA256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f

# renovate: datasource=github-release-attachments depName=rust-secure-code/cargo-auditable
CARGO_AUDITABLE_VERSION=v0.7.6
CARGO_AUDITABLE_SHA256=42b66c852fbb9074a9ca356279a92eb753f48dde16017b8c82f48dcd05d6c856

# renovate: datasource=github-release-attachments depName=sigstore/cosign
COSIGN_VERSION=v3.1.3
COSIGN_SHA256=4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71

# renovate: datasource=github-release-attachments depName=rhysd/actionlint
ACTIONLINT_VERSION=v1.7.12
ACTIONLINT_SHA256=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8

# renovate: datasource=github-release-attachments depName=koalaman/shellcheck
SHELLCHECK_VERSION=v0.11.0
SHELLCHECK_SHA256=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198

tool_pin() {
    case $1 in
        cargo-deny)
            version=$CARGO_DENY_VERSION
            url="https://github.com/EmbarkStudios/cargo-deny/releases/download/$CARGO_DENY_VERSION/cargo-deny-$CARGO_DENY_VERSION-x86_64-unknown-linux-musl.tar.gz"
            sha=$CARGO_DENY_SHA256
            archive_member="cargo-deny-$CARGO_DENY_VERSION-x86_64-unknown-linux-musl/cargo-deny"
            ;;
        cargo-auditable)
            version=$CARGO_AUDITABLE_VERSION
            url="https://github.com/rust-secure-code/cargo-auditable/releases/download/$CARGO_AUDITABLE_VERSION/cargo-auditable-x86_64-unknown-linux-musl.tar.xz"
            sha=$CARGO_AUDITABLE_SHA256
            archive_member="cargo-auditable-x86_64-unknown-linux-musl/cargo-auditable"
            ;;
        cosign)
            version=$COSIGN_VERSION
            url="https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION/cosign-linux-amd64"
            sha=$COSIGN_SHA256
            archive_member=
            ;;
        actionlint)
            version=$ACTIONLINT_VERSION
            url="https://github.com/rhysd/actionlint/releases/download/$ACTIONLINT_VERSION/actionlint_${ACTIONLINT_VERSION#v}_linux_amd64.tar.gz"
            sha=$ACTIONLINT_SHA256
            archive_member=actionlint
            ;;
        shellcheck)
            version=$SHELLCHECK_VERSION
            url="https://github.com/koalaman/shellcheck/releases/download/$SHELLCHECK_VERSION/shellcheck-$SHELLCHECK_VERSION.linux.x86_64.tar.xz"
            sha=$SHELLCHECK_SHA256
            archive_member="shellcheck-$SHELLCHECK_VERSION/shellcheck"
            ;;
        *) usage ;;
    esac
}

main() {
    (($# == 2)) || usage
    local version url sha archive_member
    if [[ $1 == --pin ]]; then
        tool_pin "$2"
        printf '%s %s\n' "$version" "$sha"
        return
    fi
    local tool=$1 dest=$2
    tool_pin "$tool"
    [[ $(uname -m) == x86_64 ]] || die "$tool: only x86_64 binaries are pinned"
    require curl sha256sum tar install

    local tmp
    tmp=$(mktemp -d)
    # shellcheck disable=SC2064 # expand now: $tmp is local and gone when EXIT fires.
    trap "rm -rf '$tmp'" EXIT
    local file="$tmp/${url##*/}"
    retry 4 5 curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --output "$file" "$url"
    printf '%s  %s\n' "$sha" "$file" | sha256sum --check --strict --quiet ||
        die "$tool: SHA-256 mismatch for $url (expected $sha)"

    mkdir -p "$dest"
    if [[ -n $archive_member ]]; then
        tar -xf "$file" -C "$tmp" "$archive_member"
        install -m 0755 "$tmp/$archive_member" "$dest/$tool"
    else
        install -m 0755 "$file" "$dest/$tool"
    fi
    log "installed $tool $version from $url (sha256 $sha) to $dest/$tool"
}

main "$@"
