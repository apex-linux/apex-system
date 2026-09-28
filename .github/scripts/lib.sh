# shellcheck shell=bash disable=SC2034 # constants and helpers for the scripts that source this file
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Shared helpers for the scripts in .github/scripts. Source it, do not run it:
#
#   . "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
#
# Every function works both on a GitHub runner and on a developer machine (the Fedora 45
# builder VM of ADR 0013). On a runner, messages become workflow annotations and
# outputs go to $GITHUB_OUTPUT; locally they go to stderr and stdout.

# Names from docs/specs/M1-IMPLEMENTATION.md section 1. Change them there first.
readonly APEX_FEDORA=45
readonly APEX_REGISTRY=ghcr.io
readonly APEX_NAMESPACE=ghcr.io/sekirokenjii
readonly APEX_REPO_TESTING=ghcr.io/sekirokenjii/apex-testing
readonly APEX_REPO_STABLE=ghcr.io/sekirokenjii/apex
readonly APEX_REPO_RPMS=ghcr.io/sekirokenjii/apex-rpms
readonly APEX_REPO_LIBFPRINT=ghcr.io/sekirokenjii/apex-libfprint
readonly APEX_SOURCE_URL=https://github.com/SekiroKenjii/apex-fedora
readonly APEX_LABEL_PREFIX=io.github.sekirokenjii.apex

APEX_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly APEX_ROOT

in_github_actions() { [[ ${GITHUB_ACTIONS:-} == true ]]; }

log() { printf '%s\n' "$*" >&2; }

notice() {
    if in_github_actions; then printf '::notice::%s\n' "$*" >&2; else printf 'notice: %s\n' "$*" >&2; fi
}

warn() {
    if in_github_actions; then printf '::warning::%s\n' "$*" >&2; else printf 'warning: %s\n' "$*" >&2; fi
}

die() {
    if in_github_actions; then printf '::error::%s\n' "$*" >&2; else printf 'error: %s\n' "$*" >&2; fi
    exit 1
}

# group TITLE / endgroup: collapsible log sections on a runner, plain headers locally.
group() { if in_github_actions; then printf '::group::%s\n' "$*" >&2; else printf '== %s\n' "$*" >&2; fi; }
endgroup() { if in_github_actions; then printf '::endgroup::\n' >&2; fi; }

# usage: print the calling script's header comment (after the SPDX line) and exit 2.
usage() {
    awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0" >&2
    exit 2
}

# require CMD...: fail early with the list of missing tools.
require() {
    local missing=() c
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || missing+=("$c")
    done
    ((${#missing[@]} == 0)) || die "missing required tools: ${missing[*]}"
}

# as_root CMD...: run CMD as root (directly when already root, else through sudo).
as_root() {
    if ((EUID == 0)); then "$@"; else sudo "$@"; fi
}

# set_output NAME VALUE: a step output on a runner, NAME=VALUE on stdout elsewhere.
set_output() {
    local name=$1 value=$2
    [[ $value != *$'\n'* ]] || die "set_output $name: multi-line values are not supported"
    if [[ -n ${GITHUB_OUTPUT:-} ]]; then
        printf '%s=%s\n' "$name" "$value" >>"$GITHUB_OUTPUT"
    else
        printf '%s=%s\n' "$name" "$value"
    fi
}

# summary LINE...: append Markdown to the job summary (stderr when not on a runner).
summary() {
    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
        printf '%s\n' "$@" >>"$GITHUB_STEP_SUMMARY"
    else
        printf '%s\n' "$@" >&2
    fi
}

# retry ATTEMPTS DELAY CMD...: run CMD until it succeeds, doubling DELAY (seconds)
# after each failure. Returns the last exit status.
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

# ---------------------------------------------------------------------------------------
# Versions: <fedora>.<yyyymmdd>.<run>, compared numerically field by field (contract §1).

version_valid() {
    [[ $1 =~ ^[0-9]+\.[0-9]{8}\.[0-9]+$ ]]
}

# version_gt A B: true when A is strictly greater than B. Both must be valid.
version_gt() {
    local a=$1 b=$2 i
    version_valid "$a" || die "invalid version '$a' (expected <fedora>.<yyyymmdd>.<run>)"
    version_valid "$b" || die "invalid version '$b' (expected <fedora>.<yyyymmdd>.<run>)"
    local -a va vb
    IFS=. read -r -a va <<<"$a"
    IFS=. read -r -a vb <<<"$b"
    for i in 0 1 2; do
        if ((10#${va[i]} > 10#${vb[i]})); then return 0; fi
        if ((10#${va[i]} < 10#${vb[i]})); then return 1; fi
    done
    return 1
}

# ---------------------------------------------------------------------------------------
# Registry queries with skopeo. REGISTRY_AUTH_FILE, when set, is passed explicitly so
# that the same call works under sudo.

skopeo_auth_args() {
    if [[ -n ${REGISTRY_AUTH_FILE:-} ]]; then
        printf '%s\n' "--authfile=$REGISTRY_AUTH_FILE"
    fi
}

# registry_manifest REF [FILE]: fetch the raw manifest of REF (`skopeo inspect --raw`:
# the manifest only, no config or layer blob) into FILE, or discard it. For a digest
# reference skopeo checks the bytes against the digest.
# Exit status: 0 found; 3 the registry says the manifest or repository does not exist
# ("manifest unknown", "name unknown", or a 404 while reading the manifest); 4 access
# denied (ghcr.io also answers this way for a package the token cannot see); 1 any
# other error, after retries. Only a cache lookup may read 4 as "absent"; a
# monotonic-version check must treat everything but 0 and 3 as "unknown", so a network
# error or a permission problem never looks like a missing tag.
registry_manifest() {
    local ref=$1 file=${2:-/dev/null} err rc n
    local -a auth
    mapfile -t auth < <(skopeo_auth_args)
    err=$(mktemp)
    for n in 1 2 3 4; do
        rc=0
        skopeo inspect --raw --no-tags "${auth[@]}" "docker://$ref" >"$file" 2>"$err" || rc=$?
        if ((rc == 0)); then
            rm -f "$err"
            return 0
        fi
        # Only this request's own answer counts: --raw reads no blob, so an error that
        # names a blob (whose digest may well contain "404") cannot land here.
        if grep -Eqi 'manifest unknown|name unknown|reading manifest [^ ]+ in [^ ]+: .*(404|not found)' "$err"; then
            rm -f "$err"
            return 3
        fi
        if grep -Eqi 'denied|unauthorized|authentication required' "$err"; then
            log "skopeo inspect $ref: access denied: $(tr '\n' ' ' <"$err")"
            rm -f "$err"
            return 4
        fi
        log "skopeo inspect --raw $ref failed (attempt $n): $(tr '\n' ' ' <"$err")"
        sleep $((n * ${APEX_RETRY_DELAY:-5}))
    done
    rm -f "$err"
    return 1
}

# registry_inspect REF: print `skopeo inspect` JSON (no layer download; skopeo reads the
# config blob for the labels). Exit status as registry_manifest, which decides whether
# REF exists: once the manifest is known to exist, any failure of the full inspect (a
# 5xx or timeout on the config blob, say) is 1 (unknown), never 3.
registry_inspect() {
    local ref=$1 out err rc n
    rc=0
    registry_manifest "$ref" || rc=$?
    ((rc == 0)) || return "$rc"
    local -a auth
    mapfile -t auth < <(skopeo_auth_args)
    err=$(mktemp)
    for n in 1 2 3 4; do
        rc=0
        out=$(skopeo inspect --no-tags "${auth[@]}" "docker://$ref" 2>"$err") || rc=$?
        if ((rc == 0)); then
            rm -f "$err"
            printf '%s\n' "$out"
            return 0
        fi
        log "skopeo inspect $ref failed (attempt $n): $(tr '\n' ' ' <"$err")"
        sleep $((n * ${APEX_RETRY_DELAY:-5}))
    done
    rm -f "$err"
    return 1
}

# registry_tag_absent REPO TAG: true only when `skopeo list-tags` succeeds and does not
# list TAG, or reports that REPO itself does not exist. A tag move takes its "first
# publication" branch only after this second, independent answer.
registry_tag_absent() {
    local repo=$1 tag=$2 out err rc=0
    local -a auth
    mapfile -t auth < <(skopeo_auth_args)
    err=$(mktemp)
    out=$(retry 3 "${APEX_RETRY_DELAY:-5}" skopeo list-tags "${auth[@]}" "docker://$repo" 2>"$err") || rc=$?
    if ((rc != 0)); then
        if grep -Eqi 'name unknown' "$err"; then
            rm -f "$err"
            return 0
        fi
        log "skopeo list-tags $repo failed: $(tr '\n' ' ' <"$err")"
        rm -f "$err"
        return 1
    fi
    rm -f "$err"
    jq -e --arg t "$tag" '(.Tags // []) | index($t) == null' <<<"$out" >/dev/null
}

# registry_digest REF: print the manifest digest of REF; same exit codes as above.
registry_digest() {
    local json rc=0
    json=$(registry_inspect "$1") || rc=$?
    ((rc == 0)) || return "$rc"
    jq -er '.Digest' <<<"$json"
}

# registry_label REF LABEL: print one label value ("" when unset); same exit codes.
registry_label() {
    local json rc=0
    json=$(registry_inspect "$1") || rc=$?
    ((rc == 0)) || return "$rc"
    jq -r --arg l "$2" '.Labels[$l] // ""' <<<"$json"
}

# registry_login: log in to ghcr.io with $GH_TOKEN (the job's GITHUB_TOKEN) and export
# REGISTRY_AUTH_FILE, which skopeo, podman and buildah all read. A no-op without a token.
registry_login() {
    if [[ -z ${GH_TOKEN:-} ]]; then
        log "GH_TOKEN is not set; registry access stays anonymous"
        return 0
    fi
    local dir=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
    export REGISTRY_AUTH_FILE="$dir/apex-registry-auth.json"
    printf '%s' "$GH_TOKEN" |
        skopeo login --authfile "$REGISTRY_AUTH_FILE" \
            --username "${GITHUB_ACTOR:-apex-ci}" --password-stdin "$APEX_REGISTRY" >&2
    chmod 0600 "$REGISTRY_AUTH_FILE"
    if [[ -n ${GITHUB_ENV:-} ]]; then
        printf 'REGISTRY_AUTH_FILE=%s\n' "$REGISTRY_AUTH_FILE" >>"$GITHUB_ENV"
    fi
}

# is_digest S: a manifest digest as written by skopeo and bootc.
is_digest() { [[ $1 =~ ^sha256:[0-9a-f]{64}$ ]]; }

# is_pinned_image REF: registry/repo[:tag]@sha256:<digest>.
is_pinned_image() { [[ $1 =~ ^[a-z0-9.-]+(/[a-z0-9._-]+)+(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$ ]]; }

# oci_tag S: S made safe for an OCI tag ([A-Za-z0-9_][A-Za-z0-9_.-]{0,127}).
oci_tag() {
    local t
    t=$(printf '%s' "$1" | tr -c 'A-Za-z0-9_.-' '_')
    t=${t:0:128}
    [[ $t =~ ^[A-Za-z0-9_] ]] || t="_${t:0:127}"
    printf '%s\n' "$t"
}

# ---------------------------------------------------------------------------------------
# image/Containerfile and image/packages are owned by the image build; CI reads them
# instead of repeating their values.

readonly APEX_CONTAINERFILE="$APEX_ROOT/image/Containerfile"
readonly APEX_TESTING_ALLOWLIST="$APEX_ROOT/image/packages/updates-testing-allowlist.txt"

# containerfile_arg NAME [FILE]: the default of `ARG NAME=value` in FILE (default
# image/Containerfile; first occurrence, quotes removed). Empty when there is none.
containerfile_arg() {
    local file=${2:-$APEX_CONTAINERFILE}
    [[ -f $file ]] || die "missing $file"
    sed -n "s/^[[:space:]]*ARG[[:space:]]\\{1,\\}$1=\\(.*\\)\$/\\1/p" "$file" |
        head -n 1 | sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\\(.*\\)'\$/\\1/"
}

# containerfile_from_images FILE [NAME=VALUE...]: every FROM reference in FILE that
# names an image (not scratch, not a stage), with ${NAME} and $NAME expanded from the
# ARG defaults in FILE, overridden by the NAME=VALUE arguments (the build args).
# References that still contain a variable are skipped.
containerfile_from_images() {
    local file=$1 line k ref name
    shift
    [[ -f $file ]] || die "missing $file"
    local -A val=() stages=()
    while IFS= read -r line; do
        k=${line%%=*}
        [[ -v "val[$k]" ]] || val[$k]=${line#*=}
    done < <(sed -n 's/^[[:space:]]*ARG[[:space:]]\{1,\}\([A-Za-z_][A-Za-z0-9_]*\)=\(.*\)$/\1=\2/p' "$file" |
        sed -e 's/[[:space:]]*$//' -e 's/="\(.*\)"$/=\1/')
    for line in "$@"; do val[${line%%=*}]=${line#*=}; done
    local -a refs=() words
    while IFS= read -r line; do
        # FROM [--flag=...]... ref [AS name]
        read -r -a words <<<"$line"
        words=("${words[@]:1}")
        while ((${#words[@]})) && [[ ${words[0]} == --* ]]; do words=("${words[@]:1}"); done
        ((${#words[@]})) || continue
        refs+=("${words[0]}")
        if ((${#words[@]} >= 3)) && [[ ${words[1]^^} == AS ]]; then stages[${words[2]}]=1; fi
    done < <(grep -Ei '^[[:space:]]*FROM[[:space:]]' "$file")
    for ref in "${refs[@]}"; do
        for k in "${!val[@]}"; do ref=${ref//"\${$k}"/${val[$k]}}; done
        while [[ $ref =~ \$([A-Za-z_][A-Za-z0-9_]*) ]]; do
            name=${BASH_REMATCH[1]}
            [[ -v "val[$name]" ]] || break
            ref=${ref/"\$$name"/${val[$name]}}
        done
        [[ $ref == *'$'* || $ref == scratch || -v "stages[$ref]" ]] && continue
        printf '%s\n' "$ref"
    done | LC_ALL=C sort -u
}

# ga_reached: 0 or 1. APEX_GA_REACHED overrides the Containerfile's ARG GA_REACHED
# default, which is the reviewed switch (set it to 1 at Fedora GA).
ga_reached() {
    local v=${APEX_GA_REACHED:-}
    [[ -n $v ]] || v=$(containerfile_arg GA_REACHED)
    [[ -n $v ]] || v=0
    [[ $v == 0 || $v == 1 ]] || die "GA_REACHED must be 0 or 1, got '$v'"
    printf '%s\n' "$v"
}

# testing_includepkgs: the updates-testing allowlist as a comma-separated includepkgs
# value (comments and blank lines dropped). Empty when the list is empty.
testing_includepkgs() {
    [[ -f $APEX_TESTING_ALLOWLIST ]] || return 0
    sed -e 's/#.*//' -e 's/[[:space:]]//g' "$APEX_TESTING_ALLOWLIST" | sed '/^$/d' | paste -sd, -
}

# dnf_repo_args: the dnf5 options (one per line) that select the image compose's
# repositories (ARCHITECTURE section 3): fedora and updates, plus updates-testing
# restricted to the allowlisted builds only while GA_REACHED=0 and the allowlist has
# entries, as the Containerfile's apex-gnome51 stanza does. The fedora:45 container
# enables all of updates-testing before GA, so every dnf5 call that builds something
# Apex ships (or checks the compiler that does) passes these instead of the defaults.
dnf_repo_args() {
    local list
    printf '%s\n' --repo=fedora --repo=updates
    list=$(testing_includepkgs)
    if [[ $(ga_reached) == 0 && -n $list ]]; then
        printf '%s\n' --repo=updates-testing "--setopt=updates-testing.includepkgs=$list"
    fi
}

# ---------------------------------------------------------------------------------------
# Fedora base images. Every digest pin of a Fedora image names registry.fedoraproject.org,
# which keeps old digests. quay.io/fedora serves a digest only while the tag still points
# at it (Fedora moves 45 daily, and quay answers MANIFEST_UNKNOWN for the old digest
# within a day), so a quay.io pin breaks CI before Renovate's daily PR can be merged.

# is_fedora_pin REF: REF is registry.fedoraproject.org/<name>[:tag]@sha256:<digest>.
is_fedora_pin() {
    [[ $1 =~ ^registry\.fedoraproject\.org/[a-z0-9._-]+(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$ ]]
}

# ---------------------------------------------------------------------------------------
# Provenance and replay checks (JSON on stdin), kept here so selftest.sh covers them.

# The GitHub repository of Apex (the attestation identity of oci-artifact.sh lookup).
readonly APEX_GITHUB_REPO=${APEX_SOURCE_URL#https://github.com/}

# replay_run_ok REPO: true when the Actions run JSON on stdin (GET
# /repos/REPO/actions/runs/ID) is a finished image.yml run of REPO's own main branch
# that was not triggered by a pull request, so its `lock` artifact was written by
# reviewed code. ci.yml replaced image.yml and builds nothing, so no run passes now;
# lock replay goes in S2c.
replay_run_ok() {
    jq -e --arg repo "$1" '
        .path == ".github/workflows/image.yml"
        and (.event | IN("push", "schedule", "workflow_dispatch"))
        and .head_branch == "main"
        and .head_repository.full_name == $repo
        and .repository.full_name == $repo
        and .status == "completed"' >/dev/null
}

# revision_descends OLD NEW: true when commit NEW is OLD or a descendant of it in the
# checkout at APEX_ROOT (APEX_GIT_ROOT overrides it for tests); exit 1 when it is not,
# exit 2 when either is not a full commit id of this history (fetch it with
# fetch-depth: 0), so a caller can tell "older" from "unknown".
revision_descends() {
    local old=$1 new=$2 root=${APEX_GIT_ROOT:-$APEX_ROOT} r rc=0
    for r in "$old" "$new"; do
        [[ $r =~ ^[0-9a-f]{40}$ ]] || return 2
        git -c safe.directory="$root" -C "$root" cat-file -e "$r^{commit}" 2>/dev/null || return 2
    done
    git -c safe.directory="$root" -C "$root" merge-base --is-ancestor "$old" "$new" || rc=$?
    ((rc <= 1)) || return 2
    return "$rc"
}

# ---------------------------------------------------------------------------------------
# JSON transforms (filters: stdin to stdout), kept here so selftest.sh covers them.

# policy_key_root ROOT: rewrite the absolute keyPath/keyPaths of a containers-policy.json
# to live under ROOT (the committed policy, checked against the checkout's keys).
policy_key_root() {
    jq --arg root "$1" '
        def fix: if type == "string" and startswith("/") then $root + . else . end;
        walk(if type == "object" then
                (if has("keyPath") then .keyPath |= fix else . end)
                | (if has("keyPaths") then .keyPaths |= map(fix) else . end)
             else . end)'
}

# policy_single_key KEYPATH: the same policy with every sigstoreSigned requirement
# trusting only KEYPATH (qualify.sh's wrong-key test).
policy_single_key() {
    jq --arg k "$1" 'walk(if type == "object" and .type? == "sigstoreSigned"
        then del(.keyPath, .keyPaths, .keyData, .keyDatas) | .keyPath = $k else . end)'
}

# policy_rejects_by_default: true when the policy's default is exactly [reject].
policy_rejects_by_default() {
    jq -e '.default == [{"type": "reject"}]' >/dev/null
}

# policy_trust_map: the trust mapping of the containers-policy.json on stdin, one
# "repository<TAB>key path" line per trusted key, sorted. It fails, naming the problem,
# unless the policy has exactly the shape of ADR 0012, the one image/checks/keys.sh
# enforces in the image: default [reject]; only the docker transport; per repository
# exactly one sigstoreSigned requirement with signedIdentity matchRepository and either
# keyPath or keyPaths (non-empty, no key twice), and no other field.
policy_trust_map() {
    local map
    map=$(jq -r '
        def check(cond; msg): if cond then . else error(msg) end;
        check(type == "object"; "the policy is not a JSON object")
        | check((keys - ["default", "transports"]) == [];
            "unexpected top-level keys \(keys - ["default", "transports"])")
        | check(.default == [{"type": "reject"}]; "default is not exactly [{\"type\": \"reject\"}]")
        | .transports
        | check(type == "object"; "transports is not an object")
        | check(keys == ["docker"]; "transports must hold only docker, found \(keys)")
        | .docker
        | check(type == "object"; "transports.docker is not an object")
        | to_entries[]
        | .key as $repo
        | .value
        | check(type == "array" and length == 1; "\($repo): not exactly one requirement")
        | .[0]
        | check(type == "object"; "\($repo): the requirement is not an object")
        | check((keys - ["type", "keyPath", "keyPaths", "signedIdentity"]) == [];
            "\($repo): unexpected fields \(keys - ["type", "keyPath", "keyPaths", "signedIdentity"])")
        | check(.type == "sigstoreSigned"; "\($repo): type is \(.type), not sigstoreSigned")
        | check(.signedIdentity == {"type": "matchRepository"};
            "\($repo): signedIdentity is not exactly {\"type\": \"matchRepository\"}")
        | check(has("keyPath") != has("keyPaths"); "\($repo): needs exactly one of keyPath and keyPaths")
        | (if has("keyPath") then [.keyPath] else .keyPaths end)
        | check(type == "array" and length > 0; "\($repo): no key paths")
        | check(all(.[]; type == "string"); "\($repo): a key path is not a string")
        | check((unique | length) == length; "\($repo): a key is listed twice")
        | .[]
        | "\($repo)\t\(.)"') || return 1
    [[ -z $map ]] || LC_ALL=C sort <<<"$map"
}

# manifest_lock_list: /usr/share/apex/manifest.json -> one NEVRA per line, the format
# build-rootfs --lock takes (the epoch appears only when it is not 0).
manifest_lock_list() {
    jq -r '.packages[]
        | "\(.name)-\(if (.epoch // 0) == 0 then "" else "\(.epoch):" end)\(.version)-\(.release).\(.arch)"'
}

# ---------------------------------------------------------------------------------------
# Boot-counter state (contract sections 4 and 5), read by qualify.sh from the guest.
# Filters on stdin, kept here so selftest.sh covers them.

# The attempts Apply gives a new deployment: apex_counter=3 (contract section 5).
readonly APEX_BOOT_ATTEMPTS=3

# grubenv_value NAME: the value of NAME in `grub2-editenv FILE list` output. Exit status
# 1 when NAME is not set; the last assignment wins, as in GRUB.
grubenv_value() {
    awk -v key="$1" '
        index($0, key "=") == 1 { value = substr($0, length(key) + 2); found = 1 }
        END { if (!found) exit 1; print value }'
}

# grubenv_check STATE [DIGEST]: check the Apex variables in `grub2-editenv FILE list`
# output. An empty value counts as unset, as in the GRUB snippet. STATE is
#   disarmed   neither apex_target nor apex_counter is set (apex_ok is not looked at:
#              the snippet resets it to 0 at every boot, and nothing confirms an
#              ordinary boot)
#   confirmed  disarmed, and apex_ok=1: an armed boot that apex-boot-assess judged green
#   armed      exactly what Apply writes: apex_target=DIGEST apex_counter=3 apex_ok=0
# Prints what differs on one line and returns 1; prints nothing and returns 0 on a match.
grubenv_check() {
    local state=$1 digest=${2:-} env target counter ok
    env=$(cat)
    target=$(grubenv_value apex_target <<<"$env" || true)
    counter=$(grubenv_value apex_counter <<<"$env" || true)
    ok=$(grubenv_value apex_ok <<<"$env" || true)
    case $state in
        disarmed | confirmed)
            if [[ -n $target || -n $counter ]]; then
                printf 'still armed: apex_target=%s apex_counter=%s\n' \
                    "${target:-<unset>}" "${counter:-<unset>}"
                return 1
            fi
            if [[ $state == confirmed && $ok != 1 ]]; then
                printf 'apex_ok=%s, expected 1 (the armed boot was not confirmed)\n' "${ok:-<unset>}"
                return 1
            fi
            ;;
        armed)
            [[ -n $digest ]] || die "grubenv_check armed needs the target digest"
            if [[ $target != "$digest" || $counter != "$APEX_BOOT_ATTEMPTS" || $ok != 0 ]]; then
                printf 'expected apex_target=%s apex_counter=%s apex_ok=0, found apex_target=%s apex_counter=%s apex_ok=%s\n' \
                    "$digest" "$APEX_BOOT_ATTEMPTS" "${target:-<unset>}" "${counter:-<unset>}" "${ok:-<unset>}"
                return 1
            fi
            ;;
        *) die "grubenv_check: unknown state '$state' (disarmed, confirmed or armed)" ;;
    esac
}

# denylist_has DIGEST: true when /var/lib/apex/denylist.json on stdin names DIGEST.
denylist_has() {
    jq -e --arg d "$1" 'any(.digests[]?; .digest == $d)' >/dev/null
}

# health_failure_has DIGEST CHECK: true when /var/lib/apex/health-failure.json on stdin
# is about DIGEST and lists CHECK among its failed checks.
health_failure_has() {
    jq -e --arg d "$1" --arg c "$2" '.digest == $d and any(.failed_checks[]?; . == $c)' >/dev/null
}
