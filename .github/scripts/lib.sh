# shellcheck shell=bash disable=SC2034 # constants and helpers for the scripts that source this file
# SPDX-License-Identifier: GPL-3.0-or-later

readonly APEX_FEDORA=45
readonly APEX_REGISTRY=ghcr.io
readonly APEX_NAMESPACE=ghcr.io/apex-linux
readonly APEX_REPO_TESTING=ghcr.io/apex-linux/apex-testing
readonly APEX_REPO_STABLE=ghcr.io/apex-linux/apex
readonly APEX_REPO_RPMS=ghcr.io/apex-linux/apex-rpms
readonly APEX_REPO_LIBFPRINT=ghcr.io/apex-linux/apex-libfprint
readonly APEX_SOURCE_URL=https://github.com/apex-linux/apex-image
readonly APEX_LABEL_PREFIX=io.github.apex-linux.apex

APEX_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly APEX_ROOT
# The component checkouts; until the split, this one holds them all.
: "${APEX_SYSTEM_SRC:=$APEX_ROOT}"
: "${APEX_DESKTOP_SRC:=$APEX_ROOT}"

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

group() { if in_github_actions; then printf '::group::%s\n' "$*" >&2; else printf '== %s\n' "$*" >&2; fi; }
endgroup() { if in_github_actions; then printf '::endgroup::\n' >&2; fi; }

require() {
    local missing=() c
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || missing+=("$c")
    done
    ((${#missing[@]} == 0)) || die "missing required tools: ${missing[*]}"
}

as_root() {
    if ((EUID == 0)); then "$@"; else sudo "$@"; fi
}

set_output() {
    local name=$1 value=$2
    [[ $value != *$'\n'* ]] || die "set_output $name: multi-line values are not supported"
    if [[ -n ${GITHUB_OUTPUT:-} ]]; then
        printf '%s=%s\n' "$name" "$value" >>"$GITHUB_OUTPUT"
    else
        printf '%s=%s\n' "$name" "$value"
    fi
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

version_valid() {
    [[ $1 =~ ^[0-9]+\.[0-9]{8}\.[0-9]+$ ]]
}

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

# ghcr.io also answers DENIED for a package the token cannot see. Only a cache lookup may
# take DENIED as a miss; a version check must treat anything but 0 and ABSENT as unknown.
readonly REGISTRY_UNKNOWN=1 REGISTRY_ABSENT=3 REGISTRY_DENIED=4

# A flag rather than the environment, so that the same call also works under sudo.
skopeo_auth_args() {
    if [[ -n ${REGISTRY_AUTH_FILE:-} ]]; then
        printf '%s\n' "--authfile=$REGISTRY_AUTH_FILE"
    fi
}

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
        # --raw reads no blob, so a 404 here is the manifest's own answer, never a blob
        # error whose digest happens to contain "404".
        if grep -Eqi 'manifest unknown|name unknown|reading manifest [^ ]+ in [^ ]+: .*(404|not found)' "$err"; then
            rm -f "$err"
            return "$REGISTRY_ABSENT"
        fi
        if grep -Eqi 'denied|unauthorized|authentication required' "$err"; then
            log "skopeo inspect $ref: access denied: $(tr '\n' ' ' <"$err")"
            rm -f "$err"
            return "$REGISTRY_DENIED"
        fi
        log "skopeo inspect --raw $ref failed (attempt $n): $(tr '\n' ' ' <"$err")"
        sleep $((n * ${APEX_RETRY_DELAY:-5}))
    done
    rm -f "$err"
    return "$REGISTRY_UNKNOWN"
}

# The manifest alone decides whether REF exists: a config-blob failure after it is
# UNKNOWN, never ABSENT.
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
    return "$REGISTRY_UNKNOWN"
}

# False on any error: a tag move treats a tag as new only on this confirmed absence.
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

registry_digest() {
    local json rc=0
    json=$(registry_inspect "$1") || rc=$?
    ((rc == 0)) || return "$rc"
    jq -er '.Digest' <<<"$json"
}

registry_label() {
    local json rc=0
    json=$(registry_inspect "$1") || rc=$?
    ((rc == 0)) || return "$rc"
    jq -r --arg l "$2" '.Labels[$l] // ""' <<<"$json"
}

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

is_digest() { [[ $1 =~ ^sha256:[0-9a-f]{64}$ ]]; }

is_pinned_image() { [[ $1 =~ ^[a-z0-9.-]+(/[a-z0-9._-]+)+(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$ ]]; }

oci_tag() {
    local t
    t=$(printf '%s' "$1" | tr -c 'A-Za-z0-9_.-' '_')
    t=${t:0:128}
    [[ $t =~ ^[A-Za-z0-9_] ]] || t="_${t:0:127}"
    printf '%s\n' "$t"
}

readonly APEX_CONTAINERFILE="$APEX_ROOT/image/Containerfile"
readonly APEX_TESTING_ALLOWLIST="$APEX_ROOT/image/packages/updates-testing-allowlist.txt"

# containerfile_arg NAME [FILE]: the unquoted default of the first `ARG NAME=...`.
containerfile_arg() {
    local file=${2:-$APEX_CONTAINERFILE}
    [[ -f $file ]] || die "missing $file"
    sed -n "s/^[[:space:]]*ARG[[:space:]]\\{1,\\}$1=\\(.*\\)\$/\\1/p" "$file" |
        head -n 1 | sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\\(.*\\)'\$/\\1/"
}

# containerfile_from_images FILE [BUILD_ARG=VALUE...]: the images FILE's FROM lines pull,
# with ARGs expanded; stages, scratch and references with unknown ARGs are left out.
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

ga_reached() {
    local v=${APEX_GA_REACHED:-}
    [[ -n $v ]] || v=$(containerfile_arg GA_REACHED)
    [[ -n $v ]] || v=0
    [[ $v == 0 || $v == 1 ]] || die "GA_REACHED must be 0 or 1, got '$v'"
    printf '%s\n' "$v"
}

testing_includepkgs() {
    [[ -f $APEX_TESTING_ALLOWLIST ]] || return 0
    sed -e 's/#.*//' -e 's/[[:space:]]//g' "$APEX_TESTING_ALLOWLIST" | sed '/^$/d' | paste -sd, -
}

# The image compose's repositories, as dnf5 options. The fedora:45 container enables all
# of updates-testing before GA, so every dnf5 call that builds what Apex ships uses these.
dnf_repo_args() {
    local list
    printf '%s\n' --repo=fedora --repo=updates
    list=$(testing_includepkgs)
    if [[ $(ga_reached) == 0 && -n $list ]]; then
        printf '%s\n' --repo=updates-testing "--setopt=updates-testing.includepkgs=$list"
    fi
}

# Only registry.fedoraproject.org keeps old Fedora digests: quay.io/fedora forgets one
# within a day of the tag moving, before Renovate's daily PR can merge.
is_fedora_pin() {
    [[ $1 =~ ^registry\.fedoraproject\.org/[a-z0-9._-]+(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$ ]]
}

readonly APEX_GITHUB_REPO=${APEX_SOURCE_URL#https://github.com/}

# Only a lock that reviewed code wrote (a finished main-branch run of REPO) may be replayed.
replay_run_ok() {
    jq -e --arg repo "$1" '
        .path == ".github/workflows/image.yml"
        and (.event | IN("push", "schedule", "workflow_dispatch"))
        and .head_branch == "main"
        and .head_repository.full_name == $repo
        and .repository.full_name == $repo
        and .status == "completed"' >/dev/null
}

# revision_descends OLD NEW: 0 when NEW is OLD or descends from it, 1 when not, 2 when
# either is not a full commit id of this checkout's history (a shallow clone?).
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

policy_key_root() {
    jq --arg root "$1" '
        def fix: if type == "string" and startswith("/") then $root + . else . end;
        walk(if type == "object" then
                (if has("keyPath") then .keyPath |= fix else . end)
                | (if has("keyPaths") then .keyPaths |= map(fix) else . end)
             else . end)'
}

policy_single_key() {
    jq --arg k "$1" 'walk(if type == "object" and .type? == "sigstoreSigned"
        then del(.keyPath, .keyPaths, .keyData, .keyDatas) | .keyPath = $k else . end)'
}

policy_rejects_by_default() {
    jq -e '.default == [{"type": "reject"}]' >/dev/null
}

# Accepts exactly the policy shape that image/checks/keys.sh enforces in the image.
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

manifest_lock_list() {
    jq -r '.packages[]
        | "\(.name)-\(if (.epoch // 0) == 0 then "" else "\(.epoch):" end)\(.version)-\(.release).\(.arch)"'
}

# The apex_counter that `apexctl update apply` writes.
readonly APEX_BOOT_ATTEMPTS=3

# The last assignment wins, as in GRUB.
grubenv_value() {
    awk -v key="$1" '
        index($0, key "=") == 1 { value = substr($0, length(key) + 2); found = 1 }
        END { if (!found) exit 1; print value }'
}

# An empty value is unset, as in GRUB's snippet; disarmed ignores apex_ok, which the
# snippet resets at every boot.
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

denylist_has() {
    jq -e --arg d "$1" 'any(.digests[]?; .digest == $d)' >/dev/null
}

health_failure_has() {
    jq -e --arg d "$1" --arg c "$2" '.digest == $d and any(.failed_checks[]?; . == $c)' >/dev/null
}
