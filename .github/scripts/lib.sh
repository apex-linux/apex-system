# shellcheck shell=bash disable=SC2034 # constants and helpers for the scripts that source this file
# SPDX-License-Identifier: GPL-3.0-or-later

readonly APEX_FEDORA=45

APEX_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly APEX_ROOT

in_github_actions() { [[ ${GITHUB_ACTIONS:-} == true ]]; }

log() { printf '%s\n' "$*" >&2; }

warn() {
    if in_github_actions; then printf '::warning::%s\n' "$*" >&2; else printf 'warning: %s\n' "$*" >&2; fi
}

die() {
    if in_github_actions; then printf '::error::%s\n' "$*" >&2; else printf 'error: %s\n' "$*" >&2; fi
    exit 1
}

group() { if in_github_actions; then printf '::group::%s\n' "$*" >&2; else printf '== %s\n' "$*" >&2; fi; }
endgroup() { if in_github_actions; then printf '::endgroup::\n' >&2; fi; }

require() {
    local missing=() c
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || missing+=("$c")
    done
    ((${#missing[@]} == 0)) || die "missing required tools: ${missing[*]}"
}

summary() {
    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
        printf '%s\n' "$@" >>"$GITHUB_STEP_SUMMARY"
    else
        printf '%s\n' "$@" >&2
    fi
}

retry() {
    local attempts=$1 delay=$2 n=1 rc
    shift 2
    while true; do
        rc=0
        "$@" || rc=$?
        ((rc == 0)) && return 0
        if ((n >= attempts)); then
            warn "giving up after $n attempts (exit $rc): $*"
            return "$rc"
        fi
        log "attempt $n/$attempts failed (exit $rc), retrying in ${delay}s: $*"
        sleep "$delay"
        n=$((n + 1))
        delay=$((delay * 2))
    done
}
