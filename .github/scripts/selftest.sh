#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Against fakes only: no network, no root, no KVM.

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require jq awk sed mktemp

FAILS=0
TESTS=0
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

pass() { TESTS=$((TESTS + 1)); }
flunk() {
    TESTS=$((TESTS + 1))
    FAILS=$((FAILS + 1))
    printf 'FAIL: %s\n' "$*" >&2
}

check() {
    local name=$1 expected=$2 actual=$3
    if [[ $expected == "$actual" ]]; then pass; else flunk "$name: expected '$expected', got '$actual'"; fi
}

check_true() {
    local name=$1
    shift
    if (set +e; "$@") >/dev/null 2>&1; then pass; else flunk "$name: expected success"; fi
}
check_false() {
    local name=$1
    shift
    if (set +e; "$@") >/dev/null 2>&1; then flunk "$name: expected failure"; else pass; fi
}

# --- versions --------------------------------------------------------------------------
check_true "valid version" version_valid 45.20260924.117
check_false "version without run" version_valid 45.20260924
check_false "version with short date" version_valid 45.2026092.1
check_false "version with letters" version_valid 45.20260924.1a
check_true "run number is numeric, not lexical" version_gt 45.20260924.10 45.20260924.9
check_true "date beats run" version_gt 45.20260925.1 45.20260924.999
check_true "fedora major beats date" version_gt 46.20260101.1 45.20261231.9
check_false "equal is not greater" version_gt 45.20260924.117 45.20260924.117
check_false "older is not greater" version_gt 45.20260923.500 45.20260924.1
check_true "leading zeros are decimal" version_gt 45.20260924.010 45.20260924.9
check_false "invalid operand dies" version_gt 45.20260924.1 garbage

# --- digests and tags ------------------------------------------------------------------
check_true "digest" is_digest "sha256:$(printf '%064d' 0)"
check_false "short digest" is_digest sha256:abc
check_false "uppercase digest" is_digest "sha256:$(printf 'A%.0s' {1..64})"
check "tag unchanged" "libfprint-1.94.10-1.fc45-s0123456789ab" "$(oci_tag libfprint-1.94.10-1.fc45-s0123456789ab)"
check "tag sanitised" "libfprint-1.94.10_rc1-1.fc45" "$(oci_tag 'libfprint-1.94.10~rc1-1.fc45')"
check "tag leading dot" "_.hidden" "$(oci_tag .hidden)"
check "tag length" 128 "$(oci_tag "$(printf 'x%.0s' {1..300})" | tr -d '\n' | wc -c)"

# --- Containerfile parsing ---------------------------------------------------------------
cat >"$TMP/Containerfile" <<'EOF'
ARG FEDORA=45
ARG BASE=registry.fedoraproject.org/fedora-bootc@sha256:5225b8afb65136bcf6131eb2b02ef7923a64e37264e5e0fdda47dfeef51851c1
ARG GA_REACHED="0"
ARG EMPTY=
ARG COMPOSE=build-rootfs
FROM registry.fedoraproject.org/fedora:${FEDORA} AS repos
FROM ${BASE} AS builder
FROM scratch AS rootfs-build-rootfs
COPY --from=builder /target-rootfs/ /
FROM --platform=linux/amd64 $BASE AS rootfs-dnf
FROM rootfs-${COMPOSE} AS final
FROM ${UNKNOWN}
EOF
check "ARG with default" "registry.fedoraproject.org/fedora-bootc@sha256:5225b8afb65136bcf6131eb2b02ef7923a64e37264e5e0fdda47dfeef51851c1" \
    "$(containerfile_arg BASE "$TMP/Containerfile")"
check "ARG quotes removed" 0 "$(containerfile_arg GA_REACHED "$TMP/Containerfile")"
check "ARG empty" "" "$(containerfile_arg EMPTY "$TMP/Containerfile")"
check "ARG missing" "" "$(containerfile_arg NOPE "$TMP/Containerfile")"
check "FROM images (stages, scratch and unresolved ARGs skipped)" \
    "registry.fedoraproject.org/fedora-bootc@sha256:5225b8afb65136bcf6131eb2b02ef7923a64e37264e5e0fdda47dfeef51851c1
registry.fedoraproject.org/fedora:45" \
    "$(containerfile_from_images "$TMP/Containerfile")"
check "FROM images with build-arg overrides" "example.org/base:1
registry.fedoraproject.org/fedora:46" \
    "$(containerfile_from_images "$TMP/Containerfile" BASE=example.org/base:1 FEDORA=46 COMPOSE=dnf)"
check_true "pinned image with tag" is_pinned_image "registry.fedoraproject.org/fedora-bootc:45@sha256:$(printf '%064d' 1)"
check_true "pinned image without tag" is_pinned_image "registry.fedoraproject.org/fedora-bootc@sha256:$(printf '%064d' 1)"
check_false "floating tag is not pinned" is_pinned_image registry.fedoraproject.org/fedora-bootc:45

is_csv_list() { [[ $1 =~ ^[A-Za-z0-9._+-]+(,[A-Za-z0-9._+-]+)*$ ]]; }
if [[ -f $APEX_TESTING_ALLOWLIST ]]; then
    check_true "includepkgs has no comments or blanks" is_csv_list "$(testing_includepkgs)"
fi

# --- policy transforms -----------------------------------------------------------------
policy='{"default":[{"type":"reject"}],"transports":{"docker":{
  "ghcr.io/sekirokenjii/apex":[{"type":"sigstoreSigned","keyPaths":["/usr/share/apex/keys/release.pub","/usr/share/apex/keys/release-backup.pub"],"signedIdentity":{"type":"matchRepository"}}],
  "ghcr.io/sekirokenjii/apex-testing":[{"type":"sigstoreSigned","keyPath":"/usr/share/apex/keys/ci.pub","signedIdentity":{"type":"matchRepository"}}],
  "example.com/data":[{"type":"sigstoreSigned","keyData":"AAAA","signedIdentity":{"type":"matchRepository"}}]}}}'
rooted=$(policy_key_root /repo/image/system_files <<<"$policy")
check "keyPaths rooted" '["/repo/image/system_files/usr/share/apex/keys/release.pub","/repo/image/system_files/usr/share/apex/keys/release-backup.pub"]' \
    "$(jq -c '.transports.docker["ghcr.io/sekirokenjii/apex"][0].keyPaths' <<<"$rooted")"
check "keyPath rooted" '/repo/image/system_files/usr/share/apex/keys/ci.pub' \
    "$(jq -r '.transports.docker["ghcr.io/sekirokenjii/apex-testing"][0].keyPath' <<<"$rooted")"
check "keyData untouched" AAAA "$(jq -r '.transports.docker["example.com/data"][0].keyData' <<<"$rooted")"
check_true "rooted policy still rejects by default" policy_rejects_by_default <<<"$rooted"

wrong=$(policy_single_key /var/tmp/wrong.pub <<<"$policy")
check "every sigstoreSigned trusts only the wrong key" '["/var/tmp/wrong.pub"]' \
    "$(jq -c '[.. | objects | select(.type? == "sigstoreSigned") | (.keyPath // "MISSING")] | unique' <<<"$wrong")"
check "no other key material left" 0 \
    "$(jq '[.. | objects | select(has("keyPaths") or has("keyData") or has("keyDatas"))] | length' <<<"$wrong")"
check_true "wrong-key policy still rejects by default" policy_rejects_by_default <<<"$wrong"
check_false "accept-anything default is refused" policy_rejects_by_default <<<'{"default":[{"type":"insecureAcceptAnything"}]}'
check_false "reject plus more is refused" policy_rejects_by_default <<<'{"default":[{"type":"reject"},{"type":"insecureAcceptAnything"}]}'

committed="$APEX_ROOT/image/system_files/usr/share/apex/containers/policy.json"
if [[ -f $committed ]]; then
    check_true "committed policy rejects by default" policy_rejects_by_default <"$committed"
    absent_keys=$(policy_key_root "$APEX_ROOT/image/system_files" <"$committed" |
        jq -r '[.. | objects | (.keyPath // empty), (.keyPaths // [] | .[])] | .[]' |
        while IFS= read -r k; do [[ -f $k ]] || printf '%s ' "$k"; done)
    check "committed policy keys exist in the checkout" "" "$absent_keys"
fi

# --- trust mapping: policy.json and image/checks/keys.sh agree exactly ---------------------
# Trust changes only in one reviewed change of this, keys.sh's TRUST_MAP and policy.json.
EXPECTED_TRUST_MAP=$'ghcr.io/sekirokenjii/apex\t/usr/share/apex/keys/release-backup.pub
ghcr.io/sekirokenjii/apex\t/usr/share/apex/keys/release.pub
ghcr.io/sekirokenjii/apex-testing\t/usr/share/apex/keys/ci.pub'
keys_sh="$APEX_ROOT/image/checks/keys.sh"
if [[ -f $keys_sh ]]; then
    keys_map=$(sed -n "/^TRUST_MAP='/,/'\$/p" "$keys_sh" | sed -e "s/^TRUST_MAP='//" -e "s/'\$//" |
        LC_ALL=C sort)
    check "image/checks/keys.sh TRUST_MAP is the ADR 0012 mapping" "$EXPECTED_TRUST_MAP" "$keys_map"
fi
if [[ -f $committed ]]; then
    policy_map=$(policy_trust_map <"$committed" 2>&1) || true
    check "the committed policy.json trusts exactly the ADR 0012 mapping" "$EXPECTED_TRUST_MAP" "$policy_map"
fi
check "policy_trust_map of the fixture policy without its extra scope" "$EXPECTED_TRUST_MAP" \
    "$(jq 'del(.transports.docker["example.com/data"])' <<<"$policy" | policy_trust_map)"
check "key order in keyPaths does not matter" "$EXPECTED_TRUST_MAP" \
    "$(jq 'del(.transports.docker["example.com/data"])
        | .transports.docker["ghcr.io/sekirokenjii/apex"][0].keyPaths |= reverse' <<<"$policy" | policy_trust_map)"
A='.transports.docker["ghcr.io/sekirokenjii/apex"]'
TT='.transports.docker["ghcr.io/sekirokenjii/apex-testing"]'
while IFS='|' read -r name edit; do
    [[ -n $name ]] || continue
    if ! edited=$(jq -c "$edit" "$committed") || [[ -z $edited ]]; then
        flunk "trust mapping: the edit '$name' does not apply"
        continue
    fi
    trust_rc=0
    trust_map=$(policy_trust_map <<<"$edited" 2>/dev/null) || trust_rc=$?
    if ((trust_rc == 0)) && [[ $trust_map == "$EXPECTED_TRUST_MAP" ]]; then
        flunk "trust mapping: '$name' is accepted as the ADR 0012 mapping"
    else
        pass
    fi
done <<EOF
CI key trusted for the stable repository|${A}[0].keyPaths += ["/usr/share/apex/keys/ci.pub"]
CI key instead of the release keys|${A}[0] |= (del(.keyPaths) | .keyPath = "/usr/share/apex/keys/ci.pub")
release key trusted for testing|${TT}[0].keyPath = "/usr/share/apex/keys/release.pub"
backup key dropped|${A}[0].keyPaths |= .[:1]
insecureAcceptAnything for the registry|.transports.docker["ghcr.io"] = [{"type": "insecureAcceptAnything"}]
second requirement in a scope|$TT += [{"type": "insecureAcceptAnything"}]
extra transport|.transports["docker-daemon"] = {"": [{"type": "insecureAcceptAnything"}]}
default accepts|.default = [{"type": "insecureAcceptAnything"}]
default reject plus accept|.default += [{"type": "insecureAcceptAnything"}]
keyData next to the key paths|${A}[0].keyData = "AAAA"
signedBy instead of sigstoreSigned|${TT}[0].type = "signedBy"
remapIdentity|${A}[0].signedIdentity = {"type": "remapIdentity", "prefix": "ghcr.io", "signedPrefix": "ghcr.io"}
matchRepoDigestOrExact|${A}[0].signedIdentity = {"type": "matchRepoDigestOrExact"}
testing scope missing|del($TT)
both keyPath and keyPaths|${TT}[0].keyPaths = ["/usr/share/apex/keys/ci.pub"]
duplicate key path|${A}[0].keyPaths += ["/usr/share/apex/keys/release.pub"]
empty keyPaths|${A}[0].keyPaths = []
extra top-level field|.specificPolicies = {}
EOF

# --- manifest lock list -----------------------------------------------------------------
manifest='{"schema":1,"packages":[
  {"name":"bash","epoch":0,"version":"5.3.0","release":"2.fc45","arch":"x86_64"},
  {"name":"shim-x64","epoch":1,"version":"16.1","release":"7","arch":"x86_64"},
  {"name":"tzdata","version":"2026b","release":"1.fc45","arch":"noarch"}]}'
check "lock list" "bash-5.3.0-2.fc45.x86_64
shim-x64-1:16.1-7.x86_64
tzdata-2026b-1.fc45.noarch" "$(manifest_lock_list <<<"$manifest")"

# --- registry error classification (fake skopeo) ---------------------------------------
mkdir -p "$TMP/bin"
cat >"$TMP/bin/skopeo" <<'EOF'
#!/usr/bin/env bash
ref=${*: -1}
raw=0
[[ " $* " == *" --raw "* ]] && raw=1
case $ref in
    *found*) printf '{"Digest":"sha256:%064d","Labels":{"org.opencontainers.image.version":"45.20260924.1"}}\n' 7 ;;
    *missing*) echo "Error: reading manifest 45 in ${ref#docker://}: manifest unknown" >&2; exit 1 ;;
    *gone404*) echo "Error: reading manifest 45 in ${ref#docker://}: StatusCode: 404, \"\"" >&2; exit 1 ;;
    *private*) echo 'Error: requested access to the resource is denied' >&2; exit 1 ;;
    # The manifest exists, but its config blob fails with a 5xx; the blob's digest
    # contains "404" and the message "not found" would too for a missing blob.
    *blob*)
        if ((raw)); then printf '{"schemaVersion":2}'; exit 0; fi
        echo 'Error: reading blob sha256:9f1c404a7e0b: fetching blob: received unexpected HTTP status: 502 Bad Gateway (blob not found in cache)' >&2
        exit 1 ;;
    *) echo 'Error: pinging container registry: dial tcp: i/o timeout' >&2; exit 1 ;;
esac
EOF
chmod +x "$TMP/bin/skopeo"
rc_of() { local rc=0; "$@" >/dev/null 2>&1 || rc=$?; printf '%s' "$rc"; }
# Run in a subshell, which keeps PATH and the retry delay; returns its failure count.
registry_checks() {
    local FAILS=0
    PATH="$TMP/bin:$PATH"
    export APEX_RETRY_DELAY=0
    check "inspect found" 0 "$(rc_of registry_inspect ghcr.io/x/found:45)"
    check "inspect missing" 3 "$(rc_of registry_inspect ghcr.io/x/missing:45)"
    check "inspect denied" 4 "$(rc_of registry_inspect ghcr.io/x/private:45)"
    check "inspect network error" 1 "$(rc_of registry_inspect ghcr.io/x/offline:45)"
    check "a 404 while reading the manifest is absent" 3 "$(rc_of registry_inspect ghcr.io/x/gone404:45)"
    check "a blob error (digest with 404) is unknown, not absent" 1 "$(rc_of registry_inspect ghcr.io/x/blob:45)"
    check "digest after a blob error is unknown" 1 "$(rc_of registry_digest ghcr.io/x/blob:45)"
    check "manifest-only probe of that image succeeds" 0 "$(rc_of registry_manifest ghcr.io/x/blob:45)"
    check "digest" "sha256:$(printf '%064d' 7)" "$(registry_digest ghcr.io/x/found:45)"
    check "label" 45.20260924.1 "$(registry_label ghcr.io/x/found:45 org.opencontainers.image.version)"
    check "absent label" "" "$(registry_label ghcr.io/x/found:45 nope)"
    return "$FAILS"
}
registry_failures=0
(registry_checks) || registry_failures=$?
FAILS=$((FAILS + registry_failures))

# --- boot-counter state (qualify.sh) ---------------------------------------------------
D1="sha256:$(printf '1%.0s' {1..64})"
D2="sha256:$(printf '2%.0s' {1..64})"
env_armed=$(printf '# GRUB Environment Block\napex_target=%s\napex_counter=3\napex_ok=0\n' "$D1")
env_counted=$(printf 'apex_target=%s\napex_counter=-1\napex_ok=0\n' "$D1")
env_confirmed=$'saved_entry=x\nboot_success=0\napex_ok=1'
env_plain=$'apex_ok=0\nboot_success=1\nboot_counter=2'
check "grubenv value" 3 "$(grubenv_value apex_counter <<<"$env_armed")"
check "grubenv value with '=' inside" "a=b" "$(grubenv_value k <<<'k=a=b')"
check "grubenv last assignment wins" 2 "$(grubenv_value k <<<$'k=1\nk=2')"
check "grubenv empty value" "" "$(grubenv_value k <<<'k=')"
check_false "grubenv unset" grubenv_value apex_target <<<"$env_plain"
check_false "grubenv prefix is not the name" grubenv_value apex <<<"$env_armed"
check_true "armed as Apply writes it" grubenv_check armed "$D1" <<<"$env_armed"
check_false "armed for another digest" grubenv_check armed "$D2" <<<"$env_armed"
check_false "counted is not freshly armed" grubenv_check armed "$D1" <<<"$env_counted"
check_false "armed needs apex_ok=0" grubenv_check armed "$D1" <<<"${env_armed/apex_ok=0/apex_ok=1}"
check_false "armed but nothing set" grubenv_check armed "$D1" <<<"$env_plain"
check_true "disarmed ignores Fedora's own variables" grubenv_check disarmed <<<"$env_plain"
check_true "disarmed with empty values" grubenv_check disarmed <<<$'apex_target=\napex_counter='
check_true "disarmed: no grubenv at all" grubenv_check disarmed <<<""
check_false "a counter alone is still armed" grubenv_check disarmed <<<'apex_counter=-1'
check_false "a target alone is still armed" grubenv_check disarmed <<<"apex_target=$D1"
check_true "confirmed" grubenv_check confirmed <<<"$env_confirmed"
check_false "confirmed needs apex_ok=1" grubenv_check confirmed <<<"$env_plain"
check_false "confirmed must be disarmed" grubenv_check confirmed <<<"${env_counted/apex_ok=0/apex_ok=1}"
check_false "unknown state dies" grubenv_check sideways <<<"$env_plain"
check "the reason names both variables" "still armed: apex_target=$D1 apex_counter=-1" \
    "$(grubenv_check disarmed <<<"$env_counted" || true)"

deny=$(printf '{"digests":[{"digest":"%s","reason":"fallback","at":1}],"extra":1}' "$D1")
check_true "denylisted" denylist_has "$D1" <<<"$deny"
check_false "not denylisted" denylist_has "$D2" <<<"$deny"
check_false "empty denylist" denylist_has "$D1" <<<'{"digests":[]}'
check_false "denylist without digests" denylist_has "$D1" <<<'{}'
health=$(printf '{"digest":"%s","failed_checks":["session","forced-red"],"at":1}' "$D1")
check_true "health failure with forced-red" health_failure_has "$D1" forced-red <<<"$health"
check_false "health failure of another digest" health_failure_has "$D2" forced-red <<<"$health"
check_false "health failure without that check" health_failure_has "$D1" failed-units <<<"$health"
check_false "health failure with no checks" health_failure_has "$D1" forced-red \
    <<<"{\"digest\":\"$D1\",\"failed_checks\":[],\"at\":1}"

# --- qualify.sh: counting the armed boots' self-reboots (fake QEMU and ssh) --------------
Q="$TMP/qualify"
mkdir -p "$Q/bin" "$Q/out/logs"
cat >"$Q/bin/qemu-system-x86_64" <<'EOF'
#!/usr/bin/env bash
# Fake QEMU: records its arguments and "boots" the next line of $FAKE_QEMU_PLAN. reboot:
# the guest reboots at once, which ends QEMU only under -no-reboot; stay: it keeps running.
set -euo pipefail
pidfile= serial= noreboot=0
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case ${args[i]} in
        -pidfile) pidfile=${args[i + 1]} ;;
        -serial) serial=${args[i + 1]#file:} ;;
        -no-reboot) noreboot=1 ;;
    esac
done
printf '%s\n' "$*" >>"$FAKE_QEMU_LOG"
plan=$(head -n 1 "$FAKE_QEMU_PLAN")
sed -i 1d "$FAKE_QEMU_PLAN"
: >"$serial"
secs=60
if [[ $plan == reboot ]] && ((noreboot)); then secs=0.3; fi
sleep "$secs" </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" >"$pidfile"
EOF
cat >"$Q/bin/ssh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_SSH_LOG"
exit 255
EOF
chmod +x "$Q/bin/qemu-system-x86_64" "$Q/bin/ssh"
: >"$Q/code.fd"
: >"$Q/vars.fd"
: >"$Q/out/red.raw"

# qualify.sh runs in a fresh bash: this shell already holds lib.sh's readonly names.
run_armed_attempts() {
    local attempt_timeout=${*: -1}
    printf '%s\n' "$@" | head -n -1 >"$Q/plan"
    : >"$Q/qemu.log"
    rm -f "$Q/ssh.log" "$Q/out/results.md" "$Q/out/logs/"*
    local rc=0
    PATH="$Q/bin:$PATH" QUALIFY_OUT="$Q/out" QUALIFY_POLL=0.1 QUALIFY_ATTEMPT_TIMEOUT="$attempt_timeout" \
        OVMF_CODE="$Q/code.fd" OVMF_VARS="$Q/vars.fd" GITHUB_ACTIONS='' \
        FAKE_QEMU_LOG="$Q/qemu.log" FAKE_QEMU_PLAN="$Q/plan" FAKE_SSH_LOG="$Q/ssh.log" \
        bash -c '. "$1"; require_kvm() { :; }; armed_attempts red "$APEX_BOOT_ATTEMPTS"' \
        _ "$APEX_ROOT/.github/scripts/qualify.sh" >"$Q/stderr.log" 2>&1 || rc=$?
    if [[ -f $Q/out/red.pid ]]; then kill "$(cat "$Q/out/red.pid")" 2>/dev/null || true; fi
    return "$rc"
}

rc=0
run_armed_attempts reboot reboot reboot stay 20 || rc=$?
check "three self-reboots, then the fallback boot keeps running" 0 "$rc"
((rc == 0)) || cat "$Q/stderr.log" >&2
check "QEMU started four times" 4 "$(wc -l <"$Q/qemu.log")"
check "every start has -no-reboot" 4 "$(grep -c -- ' -no-reboot' "$Q/qemu.log")"
check "one console log per boot" "red-attempt1.serial.log red-attempt2.serial.log red-attempt3.serial.log red-fallback.serial.log" \
    "$(find "$Q/out/logs" -type f -printf '%f\n' | sort | tr '\n' ' ' | sed 's/ $//')"
check "one result row per attempt" 3 "$(grep -c '^| red-attempt[1-3] | PASS |' "$Q/out/results.md")"
check_false "no ssh while the armed boots run" test -s "$Q/ssh.log"

rc=0
run_armed_attempts reboot stay 2 || rc=$?
check "an attempt that does not reboot fails" 1 "$rc"
check "the failing attempt is named" 1 "$(grep -c '^| red-attempt2 | FAIL |' "$Q/out/results.md" 2>/dev/null || true)"
check "no attempt after the failed one" 2 "$(wc -l <"$Q/qemu.log")"
check_false "no ssh before the failure" test -s "$Q/ssh.log"

rc=0
PATH="$Q/bin:$PATH" bash -c '. "$1"' _ "$APEX_ROOT/.github/scripts/qualify.sh" >"$Q/stderr.log" 2>&1 || rc=$?
check "sourcing qualify.sh runs nothing" "0:" "$rc:$(cat "$Q/stderr.log")"

# --- rpms.sh: cargo-auditable follows the spec and is part of the key ------------------
rpms_fn() { bash -c '. "$1"; shift; "$@"' _ "$APEX_ROOT/.github/scripts/rpms.sh" "$@"; }
printf '%%bcond auditable 1\n' >"$TMP/on.spec"
printf '  %%bcond_without auditable\n' >"$TMP/on-legacy.spec"
printf '%%bcond auditable 0\n' >"$TMP/off.spec"
printf '%%bcond_with auditable\n' >"$TMP/off-legacy.spec"
printf '# %%bcond auditable 1 would be on\n%%bcond auditable 0\n' >"$TMP/off-comment.spec"
printf '%%bcond auditable 10\n%%bcond check 1\n' >"$TMP/off-other.spec"
check "bcond auditable 1" on "$(rpms_fn spec_auditable "$TMP/on.spec")"
check "bcond_without auditable" on "$(rpms_fn spec_auditable "$TMP/on-legacy.spec")"
check "bcond auditable 0" off "$(rpms_fn spec_auditable "$TMP/off.spec")"
check "bcond_with auditable" off "$(rpms_fn spec_auditable "$TMP/off-legacy.spec")"
check "a comment does not count" off "$(rpms_fn spec_auditable "$TMP/off-comment.spec")"
check "other bconds do not count" off "$(rpms_fn spec_auditable "$TMP/off-other.spec")"
if [[ -f $APEX_ROOT/packaging/apex.spec ]]; then
    check "apex.spec builds with cargo-auditable" on "$(rpms_fn spec_auditable "$APEX_ROOT/packaging/apex.spec")"
    check "the key records install-tool.sh's cargo-auditable pin" \
        "$("$APEX_ROOT/.github/scripts/install-tool.sh" --pin cargo-auditable)" "$(rpms_fn auditable_pin)"
fi
check_true "install-tool.sh pins are '<version> <sha256>'" \
    grep -Eqx 'v?[0-9][0-9.]* [0-9a-f]{64}' <<<"$("$APEX_ROOT/.github/scripts/install-tool.sh" --pin cargo-auditable)"
check_false "install-tool.sh --pin rejects unknown tools" "$APEX_ROOT/.github/scripts/install-tool.sh" --pin nope
PIN_A="v0.7.6 $(printf 'a%.0s' {1..64})"
RECIPE_A=$'repos --repo=fedora --repo=updates\n100755 blob 1111 .github/scripts/rpms.sh'
key_a=$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 "$PIN_A" "$RECIPE_A" "tree")
check "a key is 64 hex digits" 64 "$(tr -dc '0-9a-f' <<<"$key_a" | wc -c)"
check "the key is stable" "$key_a" "$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 "$PIN_A" "$RECIPE_A" "tree")"
check_false "a cargo-auditable bump changes the key" test "$key_a" = \
    "$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 "v0.7.7 $(printf 'b%.0s' {1..64})" "$RECIPE_A" "tree")"
check_false "auditable off changes the key" test "$key_a" = "$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 off "$RECIPE_A" "tree")"
check_false "a rust update changes the key" test "$key_a" = \
    "$(rpms_fn key_of rust-1.98.2-1.fc45.x86_64 "$PIN_A" "$RECIPE_A" "tree")"
check_false "a change of rpms.sh changes the key" test "$key_a" = \
    "$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 "$PIN_A" "${RECIPE_A/1111/2222}" "tree")"
check_false "admitting updates-testing builds changes the key" test "$key_a" = \
    "$(rpms_fn key_of rust-1.98.1-1.fc45.x86_64 "$PIN_A" "${RECIPE_A/updates/updates --repo=updates-testing}" "tree")"
check "the recipe starts with the repository options" \
    "repos $(bash -c '. "$1"; dnf_repo_args' _ "$APEX_ROOT/.github/scripts/lib.sh" | paste -sd' ' -)" \
    "$(bash -c '. "$1"; git_() { printf "100755 blob 1111\t.github/scripts/rpms.sh\n"; }; recipe' _ "$APEX_ROOT/.github/scripts/rpms.sh" | head -n 1)"
check "the recipe covers rpms.sh" ".github/scripts/rpms.sh" \
    "$(bash -c '. "$1"; printf "%s\n" "${RECIPE_PATHS[*]}"' _ "$APEX_ROOT/.github/scripts/rpms.sh")"
check "the key covers the Makefile, Cargo.toml and the packaged docs" \
    "crates packaging/apex.spec Makefile Cargo.lock Cargo.toml LICENSE README.md" \
    "$(bash -c '. "$1"; printf "%s\n" "${KEY_PATHS[*]}"' _ "$APEX_ROOT/.github/scripts/rpms.sh")"
atree=$(bash -c '. "$1"; git_() { [[ $1 != ls-tree ]] || printf "100644 blob 1111\tcrates/apexctl/src/main.rs\n100644 blob 2222\tcrates/apex-lane/src/main.rs\n100644 blob 3333\tcrates/apex-lanes/x.rs\n"; }; apex_tree' _ "$APEX_ROOT/.github/scripts/rpms.sh" 2>/dev/null)
check "the apex key leaves crates/apex-lane/, the builder VM's tool, out" "100644 blob 1111	crates/apexctl/src/main.rs
100644 blob 3333	crates/apex-lanes/x.rs" "$atree"

# --- the installed layout: M1-IMPLEMENTATION.md, make install and apex.spec agree ---------
if command -v make >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    L="$TMP/layout"
    mkdir -p "$L/target/release"
    for b in apexctl apex-updated apex-boot-assess; do
        printf '#!/bin/sh\n' >"$L/target/release/$b"
        chmod +x "$L/target/release/$b"
    done
    # The Makefile's own defaults, whatever this shell exports (PREFIX, CARGO_TARGET_DIR...).
    env -i PATH="$PATH" make -s -n -C "$APEX_ROOT" install DESTDIR=/DESTDIR >"$L/dry-run" 2>&1 ||
        flunk "make -n install failed: $(cat "$L/dry-run")"
    layout_rc=0
    env -i PATH="$PATH" make -s -C "$APEX_ROOT" install DESTDIR="$L/root" \
        CARGO_TARGET_DIR="$L/target" >"$L/install.log" 2>&1 || layout_rc=$?
    check "make install into a scratch root succeeds" 0 "$layout_rc"
    ((layout_rc == 0)) || cat "$L/install.log" >&2
    (cd "$L/root" && find . -type f | sed 's/^\.//' | LC_ALL=C sort) >"$L/installed"
    layout_problems=$(python3 - "$APEX_ROOT" "$L/dry-run" "$L/installed" <<'PY'
import os, re, sys
root, dry_run, installed = sys.argv[1:4]
problems = []

# Rows: | `source`[, `.ext`] | `destination` or `directory/` |
text = open(f"{root}/docs/specs/M1-IMPLEMENTATION.md", encoding="utf-8").read()
section = re.search(r"^## 2\..*?$(.*?)^## ", text, re.M | re.S)
contract = set()
for line in (section.group(1) if section else "").splitlines():
    cells = [c.strip() for c in line.strip().strip("|").split("|")]
    if len(cells) != 2 or not cells[0].startswith("`"):
        continue
    srcs = re.findall(r"`([^`]+)`", cells[0])
    dest = re.findall(r"`([^`]+)`", cells[1])[0]
    first = srcs[0]
    for src in srcs:
        if src.startswith("."):
            src = os.path.splitext(first)[0] + src
        contract.add((src, dest + os.path.basename(src) if dest.endswith("/") else dest))
if not contract:
    problems.append("no rows found in section 2 of docs/specs/M1-IMPLEMENTATION.md")

# make -n install: install -Dm0NNN SOURCE '/DESTDIR/PATH'
made = set(re.findall(r"install -Dm0[0-7]{3} (\S+) '/DESTDIR(/[^']+)'", open(dry_run).read()))
for src, dest in sorted(contract - made):
    problems.append(f"make install does not install {src} as {dest} (contract section 2)")
for src, dest in sorted(made - contract):
    problems.append(f"make install installs {src} as {dest}, which contract section 2 lacks")
dests = {d for _, d in contract}
got = set(open(installed).read().split())
for d in sorted(dests - got):
    problems.append(f"a real make install did not produce {d}")
for d in sorted(got - dests):
    problems.append(f"a real make install produced {d}, which contract section 2 lacks")

macros = {"_bindir": "/usr/bin", "_libexecdir": "/usr/libexec", "_datadir": "/usr/share",
          "_unitdir": "/usr/lib/systemd/system", "_tmpfilesdir": "/usr/lib/tmpfiles.d",
          "_prefix": "/usr"}
spec = open(f"{root}/packaging/apex.spec", encoding="utf-8").read()
files = re.search(r"^%files\s*$(.*?)^%changelog", spec, re.M | re.S)
listed, dirs = set(), set()
for line in (files.group(1) if files else "").splitlines():
    line = line.strip()
    if not line or line.startswith("#") or line.startswith(("%license", "%doc")):
        continue
    is_dir = line.startswith("%dir ")
    path = re.sub(r"%\{(\w+)\}", lambda m: macros.get(m.group(1), m.group(0)),
                  line[5:] if is_dir else line)
    if "%" in path or not path.startswith("/"):
        problems.append(f"apex.spec %files line not understood: {line}")
    (dirs if is_dir else listed).add(path)
for d in sorted(dests - listed):
    problems.append(f"apex.spec %files does not list {d}")
for d in sorted(listed - dests):
    problems.append(f"apex.spec %files lists {d}, which make install does not produce")
for d in sorted({os.path.dirname(p) for p in dests if p.startswith("/usr/lib/systemd/system/")
                 and p.count("/") == 6} - dirs):
    problems.append(f"apex.spec %files does not own the drop-in directory {d} (%dir)")
print("\n".join(problems))
PY
)
    check "contract section 2, make install and apex.spec %files agree" "" "$layout_problems"
    check_true "the boot-counter drop-ins are part of that layout" \
        grep -qx '/usr/lib/systemd/system/emergency.service.d/50-apex-bootcount.conf' "$L/installed"
fi

# --- repositories: the compose's set, never all of updates-testing ---------------------
repo_args_with() {
    local ga_reached=$1 allowlist=$2
    APEX_GA_REACHED=$ga_reached bash -c '. "$1"; fake_allowlist=$2; testing_includepkgs() { printf "%s\n" "$fake_allowlist"; }; dnf_repo_args | paste -sd" " -' \
        _ "$APEX_ROOT/.github/scripts/lib.sh" "$allowlist"
}
check "pre-GA with an allowlist: only those builds from updates-testing" \
    "--repo=fedora --repo=updates --repo=updates-testing --setopt=updates-testing.includepkgs=gtk4-4.24.0-1.fc45,glib2-2.90.0-1.fc45" \
    "$(repo_args_with 0 gtk4-4.24.0-1.fc45,glib2-2.90.0-1.fc45)"
check "pre-GA with an empty allowlist: no updates-testing" "--repo=fedora --repo=updates" "$(repo_args_with 0 '')"
check "after GA: no updates-testing" "--repo=fedora --repo=updates" "$(repo_args_with 1 gtk4-4.24.0-1.fc45)"

F="$TMP/fprint"
mkdir -p "$F/bin"
cat >"$F/bin/rpm" <<'EOF'
#!/usr/bin/env bash
case "$*" in
    "-E %fedora") echo 45 ;;
    "-E %_arch") echo x86_64 ;;
    *) exit 1 ;;
esac
EOF
cat >"$F/bin/dnf5" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_DNF_LOG"
echo 'libfprint 0 1.94.100 1.fc45 libfprint-1.94.100-1.fc45.src.rpm'
EOF
chmod +x "$F/bin/rpm" "$F/bin/dnf5"
fprint_key() {
    : >"$F/dnf.log"
    PATH="$F/bin:$PATH" FAKE_DNF_LOG="$F/dnf.log" GA_REACHED=$1 TESTING_INCLUDEPKGS=$2 \
        "$APEX_ROOT/packaging/libfprint/build.sh" key 2>/dev/null
}
if [[ -x $APEX_ROOT/packaging/libfprint/build.sh ]]; then
    k_empty=$(fprint_key 0 '')
    check_false "libfprint, empty allowlist: updates-testing not enabled" grep -q updates-testing "$F/dnf.log"
    k_list=$(fprint_key 0 gtk4-4.24.0-1.fc45)
    check_true "libfprint, allowlist: updates-testing enabled" grep -q -- '--repo=updates-testing ' "$F/dnf.log"
    check_true "libfprint, allowlist: restricted by includepkgs" \
        grep -q -- '--setopt=updates-testing.includepkgs=gtk4-4.24.0-1.fc45' "$F/dnf.log"
    k_ga=$(fprint_key 1 gtk4-4.24.0-1.fc45)
    check_false "libfprint after GA: updates-testing not enabled" grep -q updates-testing "$F/dnf.log"
    check "libfprint key names the stock NVR" libfprint-1.94.100-1.fc45-s "${k_empty%????????????}"
    check_false "libfprint key changes with the repository options" test "$k_empty" = "$k_list"
    check "libfprint key is the same with no updates-testing either way" "$k_empty" "$k_ga"
fi

# --- desktop/shell ships inside gnome-shell under gnome-shell's licence -------------------
if [[ -d $APEX_ROOT/desktop/shell ]]; then
    unlicensed=$(cd "$APEX_ROOT/desktop/shell" && find . -type f -printf '%P\n' | LC_ALL=C sort | while IFS= read -r f; do
        [[ $(head -n 3 -- "$f") == *'SPDX-License-Identifier: GPL-2.0-or-later'* ]] || printf '%s ' "$f"
    done)
    check "desktop/shell: every file is GPL-2.0-or-later (SPDX)" "" "$unlicensed"
fi

# --- image.sh: the rebuild lane's lock in the image context ------------------------------
image_sh() { bash -c '. "$1"; shift; eval "$1"' _ "$APEX_ROOT/.github/scripts/image.sh" "$@"; }
IL="$TMP/image-lane"
mkdir -p "$IL/pkgs/gnome-shell/k1"
ilsha=$(printf 'c%.0s' {1..64})
{
    printf '# apex rpms.lock 1: written by apex-lane (packaging/README.md, "rpms.lock")\n'
    printf 'family gnome-shell built k1\n'
    printf 'rpm gnome-shell built gnome-shell gnome-shell-0:51.0-1.fc45.apex1.x86_64 %s pkgs/gnome-shell/k1/gnome-shell-51.0-1.fc45.apex1.x86_64.rpm\n' "$ilsha"
    printf 'rpm gnome-shell mirror mutter mutter-0:51.0-1.fc45.x86_64 %s pkgs/gnome-shell/k1/mutter-51.0-1.fc45.x86_64.rpm\n' "$ilsha"
    printf 'family libfprint degraded - since=2026-10-12T04:10:00Z reason=0001 does not apply\n'
} >"$IL/rpms.lock"
check "lane lock: the RPMs to copy" "pkgs/gnome-shell/k1/gnome-shell-51.0-1.fc45.apex1.x86_64.rpm $ilsha
pkgs/gnome-shell/k1/mutter-51.0-1.fc45.x86_64.rpm $ilsha" "$(image_sh "lane_lock_check '$IL/rpms.lock' gnome-shell libfprint" 2>/dev/null)"
check "lane lock: degraded families" libfprint "$(image_sh "lane_lock_states '$IL/rpms.lock' degraded")"
check "lane lock: no held family" "" "$(image_sh "lane_lock_states '$IL/rpms.lock' held")"
if [[ -f $APEX_ROOT/packaging/pkgs/gnome-shell/pkg.env ]]; then
    check "lane families: the committed pkg.env files" gnome-shell "$(image_sh lane_families | paste -sd' ' -)"
fi
bad_lane() {
    local name=$1 sed_script=$2
    shift 2
    local families=${*:-gnome-shell}
    sed -e "$sed_script" "$IL/rpms.lock" >"$IL/bad.lock"
    check "lane lock refuses $name" 1 "$(rc_of image_sh "lane_lock_check '$IL/bad.lock' $families")"
}
# shellcheck disable=SC2016 # sed scripts: $a appends a line
{
    bad_lane "a lock that misses a lane family" 's/^x//' gnome-shell nautilus
    bad_lane "another format" '1s/rpms.lock 1:/rpms.lock 2:/'
    bad_lane "an unknown state" 's/^family gnome-shell built/family gnome-shell maybe/'
    bad_lane "a repeated family" '$a family gnome-shell stock -'
    bad_lane "rpm lines of a stock family" 's/^family gnome-shell built/family gnome-shell stock/'
    bad_lane "an rpm line before its family" '2d'
    bad_lane "a built family with mirrors only" '/ built gnome-shell /d'
    bad_lane "a NEVRA of another package" 's/built gnome-shell gnome-shell-0:/built gnome-shell mutter-0:/'
    bad_lane "a NEVRA without an epoch" 's/gnome-shell-0:51/gnome-shell-51/'
    bad_lane "a short sha256" "s/$ilsha/abc/"
    bad_lane "a path outside the store" 's|pkgs/gnome-shell/k1/mutter|../mutter|'
    bad_lane "an absolute path" 's| pkgs/gnome-shell/k1/mutter| /tmp/mutter|'
    bad_lane "a file name that is not the NEVRA's" 's|/mutter-51.0-1.fc45.x86_64.rpm|/mutter-51.0-2.fc45.x86_64.rpm|'
    bad_lane "a package twice" '$a rpm gnome-shell mirror mutter mutter-0:51.0-1.fc45.x86_64 '"$ilsha"' pkgs/gnome-shell/k1/mutter-51.0-1.fc45.x86_64.rpm'
    bad_lane "an unknown record" '$a pin mutter 51.0'
}
check "lane lock refuses a missing file" 1 "$(rc_of image_sh "lane_lock_check '$IL/nope.lock' gnome-shell")"
GOLDEN=$APEX_ROOT/crates/apex-lane/tests/golden
golden_locks=0
for g in "$GOLDEN"/lock/b-*.lock "$GOLDEN"/lock/store-rpms.lock.next "$GOLDEN"/hold/*/expect/rpms.lock.next; do
    [[ -f $g ]] || continue
    golden_locks=$((golden_locks + 1))
    check "lane lock: apex-lane's golden ${g#"$GOLDEN"/} is accepted" 0 "$(rc_of image_sh "lane_lock_check '$g' gnome-shell")"
done
check "lane lock: apex-lane's golden locks are there" 1 "$((golden_locks > 0))"
mkdir -p "$IL/rpms"
: >"$IL/rpms/apex-0.1.0-1.fc45.x86_64.rpm"
(cd "$IL/rpms" && sha256sum -- *.rpm >SHA256SUMS)
ctx_rc=0
ctx_log=$(APEX_WORK="$IL/work" GITHUB_ACTIONS='' "$APEX_ROOT/.github/scripts/image.sh" context --rpms-dir "$IL/rpms" 2>&1) || ctx_rc=$?
check "image.sh context without --lane-lock fails" 1 "$ctx_rc"
check_true "and says the lane's lock is required" grep -q "the rebuild lane's lock is required" <<<"$ctx_log"

# --- rpms.sh: the apex-desktop key ----------------------------------------------------------
check "the apex-desktop key covers desktop/, its spec, the Makefile and LICENSE" \
    "desktop packaging/apex-desktop.spec Makefile LICENSE" \
    "$(bash -c '. "$1"; printf "%s\n" "${DESKTOP_KEY_PATHS[*]}"' _ "$APEX_ROOT/.github/scripts/rpms.sh")"
dkey=$(rpms_fn desktop_key_of "$RECIPE_A" "tree")
check "an apex-desktop key is 64 hex digits" 64 "$(tr -dc '0-9a-f' <<<"$dkey" | wc -c)"
check_false "a desktop change changes the apex-desktop key" test "$dkey" = "$(rpms_fn desktop_key_of "$RECIPE_A" "tree2")"
check_false "a recipe change changes the apex-desktop key" test "$dkey" = "$(rpms_fn desktop_key_of "${RECIPE_A/1111/2222}" "tree")"
check_false "the apex-desktop key is not the apex key" test "$dkey" = "$key_a"
dtree=$(bash -c '. "$1"; git_() { [[ $1 != ls-tree ]] || printf "100644 blob 1111\tdesktop/schemas/x.gschema.xml\n100644 blob 2222\tdesktop/shell/js/ui/apex/hooks.js\n100644 blob 3333\tMakefile\n"; }; desktop_tree' _ "$APEX_ROOT/.github/scripts/rpms.sh" 2>/dev/null)
check "the apex-desktop key leaves desktop/shell/ to the lane" "100644 blob 1111	desktop/schemas/x.gschema.xml
100644 blob 3333	Makefile" "$dtree"

# --- apex-desktop.spec installs every file of desktop/ outside desktop/shell/ -------------------
if [[ -f $APEX_ROOT/packaging/apex-desktop.spec ]]; then
    unpackaged=$(cd "$APEX_ROOT" && git ls-files --cached --others --exclude-standard -- desktop ':(exclude)desktop/shell' ':(exclude)desktop/README.md' |
        while IFS= read -r f; do
            grep -Fq -- "$f" packaging/apex-desktop.spec || printf '%s ' "$f"
        done)
    check "apex-desktop.spec installs every desktop/ file" "" "$unpackaged"
fi

# --- rust-toolchain-check.sh --warn-only (fake rustc) ----------------------------------
mkdir -p "$TMP/rustc-bin"
printf '#!/usr/bin/env bash\nprintf "rustc 9.9.9\\nrelease: 9.9.9\\n"\n' >"$TMP/rustc-bin/rustc"
chmod +x "$TMP/rustc-bin/rustc"
check "toolchain mismatch fails ci.yml" 1 \
    "$(PATH="$TMP/rustc-bin:$PATH" GITHUB_STEP_SUMMARY=/dev/null rc_of "$APEX_ROOT/.github/scripts/rust-toolchain-check.sh")"
check "toolchain mismatch only warns in rpms.sh build" 0 \
    "$(PATH="$TMP/rustc-bin:$PATH" GITHUB_STEP_SUMMARY=/dev/null rc_of "$APEX_ROOT/.github/scripts/rust-toolchain-check.sh" --warn-only)"
check "unknown option is a usage error" 2 \
    "$(PATH="$TMP/rustc-bin:$PATH" rc_of "$APEX_ROOT/.github/scripts/rust-toolchain-check.sh" --nope)"

# --- Fedora images: digest pins on registry.fedoraproject.org only ------------------------
DQ="sha256:$(printf 'd%.0s' {1..64})"
check_true "a registry.fedoraproject.org digest pin" is_fedora_pin "registry.fedoraproject.org/fedora-bootc@$DQ"
check_true "a registry.fedoraproject.org digest pin with a tag" is_fedora_pin "registry.fedoraproject.org/fedora:45@$DQ"
check_false "a quay.io digest pin is refused" is_fedora_pin "quay.io/fedora/fedora-bootc@$DQ"
check_false "a floating tag is refused" is_fedora_pin registry.fedoraproject.org/fedora:45
check_false "another registry is refused" is_fedora_pin "ghcr.io/x/fedora@$DQ"
check_false "a lookalike host is refused" is_fedora_pin "registry.fedoraproject.org.evil.example/fedora@$DQ"
check_false "a nested repository is refused" is_fedora_pin "registry.fedoraproject.org/x/fedora@$DQ"
if [[ -f $APEX_CONTAINERFILE ]]; then
    for arg in BASE REPOS_BASE; do
        check_true "image/Containerfile ARG $arg is a registry.fedoraproject.org digest pin" \
            is_fedora_pin "$(containerfile_arg "$arg")"
    done
fi
IB="$TMP/image-build"
mkdir -p "$IB/bin" "$IB/work" "$IB/tree/.github" "$IB/tree/image"
cp -R "$APEX_ROOT/.github/scripts" "$IB/tree/.github/"
cat >"$IB/bin/podman" <<'EOF'
#!/usr/bin/env bash
echo "unexpected: $0 $*" >&2
exit 99
EOF
cp "$IB/bin/podman" "$IB/bin/git"
chmod +x "$IB/bin/podman" "$IB/bin/git"
: >"$IB/work/inputs.env"
image_build_with() {
    local sed_script=$1
    sed "$sed_script" "$APEX_CONTAINERFILE" >"$IB/tree/image/Containerfile"
    ib_rc=0
    ib_log=$(PATH="$IB/bin:$PATH" APEX_WORK="$IB/work" GITHUB_ACTIONS='' \
        "$IB/tree/.github/scripts/image.sh" build 2>&1) || ib_rc=$?
}
for arg in BASE REPOS_BASE; do
    image_build_with "s|^ARG $arg=registry\.fedoraproject\.org/|ARG $arg=quay.io/fedora/|"
    check "image.sh build refuses ARG $arg on quay.io" 1 "$ib_rc"
    check_true "and says it must be on registry.fedoraproject.org ($arg)" \
        grep -q "ARG $arg in image/Containerfile must be a digest pin on registry.fedoraproject.org" <<<"$ib_log"
    check_false "and never reaches podman or git ($arg)" grep -q unexpected <<<"$ib_log"
done
image_build_with 's|^ARG REPOS_BASE=.*|ARG REPOS_BASE=|'
check "image.sh build refuses an empty REPOS_BASE" 1 "$ib_rc"
image_build_with 's|^\(ARG BASE=registry\.fedoraproject\.org/fedora-bootc\)@.*|\1:45|'
check "image.sh build refuses a floating BASE tag" 1 "$ib_rc"
# Past the check, image.sh build calls the fake git for the revision.
image_build_with 's|^$||'
check_true "the committed Containerfile passes the check" grep -q 'unexpected: .*git' <<<"$ib_log"
wf_quay=$(grep -rhoE 'quay\.io/fedora/[a-z0-9._-]+(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}' \
    "$APEX_ROOT/.github/workflows" || true)
check "no workflow pins a Fedora image by digest on quay.io" "" "$wf_quay"
wf_unpinned=$(grep -rhE '^[[:space:]]*image:[[:space:]]' "$APEX_ROOT/.github/workflows" |
    grep -vE 'image: registry\.fedoraproject\.org/fedora:45@sha256:[0-9a-f]{64}$' || true)
check "every workflow container is registry.fedoraproject.org/fedora:45 by digest" "" "$wf_unpinned"
quay_refs=$(grep -nE '^[[:space:]]*[^#[:space:]].*quay' "$APEX_ROOT"/.github/scripts/*.sh \
    "$APEX_ROOT"/.github/workflows/*.yml "$APEX_ROOT/Justfile" "$APEX_CONTAINERFILE" |
    grep -v '/selftest\.sh:' || true)
check "quay.io is used nowhere in scripts, workflows, the Justfile or the Containerfile" "" "$quay_refs"

# --- renovate.json: every digest pin has a manager, and the Fedora rule covers them ----
if command -v python3 >/dev/null 2>&1; then
    renovate_problems=$(python3 - "$APEX_ROOT" <<'PY'
import glob, json, re, sys
root = sys.argv[1]
cfg = json.load(open(f"{root}/renovate.json"))
problems = []
# Renovate's regex managers use JavaScript named groups; Python spells them (?P<...>).
def py(rx):
    return re.compile(rx.replace("(?<", "(?P<"))
containerfile = open(f"{root}/image/Containerfile").read()
managers = [m for m in cfg.get("customManagers", [])
            if any(re.search(p.strip("/"), "image/Containerfile") for p in m.get("managerFilePatterns", []))]
fedora_rule = [r for r in cfg["packageRules"] if r.get("groupName") == "fedora 45 images"]
names = set(fedora_rule[0]["matchPackageNames"]) if fedora_rule else set()
frozen = [r for r in cfg["packageRules"] if r.get("enabled") is False and "docker" in r.get("matchDatasources", [])]
if not frozen or set(frozen[0]["matchPackageNames"]) != names:
    problems.append("the rule that disables Fedora major/minor/patch updates lists other names than 'fedora 45 images'")
for line in containerfile.splitlines():
    if not re.match(r"^ARG [A-Z_]+=\S+@sha256:[0-9a-f]{64}\s*$", line):
        continue
    hit = None
    for m in managers:
        for rx in m["matchStrings"]:
            g = py(rx).search(line)
            if g:
                hit = (m, g)
    if not hit:
        problems.append(f"no custom manager matches: {line}")
        continue
    m, g = hit
    new = "sha256:" + "e" * 64
    out = m["autoReplaceStringTemplate"].replace("{{{depName}}}", g.group("depName")).replace("{{{newDigest}}}", new)
    if out != line.strip().replace(g.group("currentDigest"), new):
        problems.append(f"replacement template rewrites '{line}' as '{out}'")
    if g.group("depName") not in names:
        problems.append(f"{g.group('depName')} is not in the 'fedora 45 images' rule")
for rule in cfg["packageRules"]:
    for name in rule.get("matchPackageNames", []):
        if "quay" in name:
            problems.append(f"a package rule still names {name}")
for m in cfg.get("customManagers", []):
    for rx in m.get("matchStrings", []):
        if "quay" in rx:
            problems.append(f"a custom manager still accepts quay.io: {rx}")
wf = "\n".join(open(p).read() for p in glob.glob(f"{root}/.github/workflows/*.yml"))
for dep in set(re.findall(r"image: (\S+?):45@sha256:", wf)):
    if dep not in names:
        problems.append(f"{dep} (a workflow container) is not in the 'fedora 45 images' rule")
print("\n".join(problems))
PY
)
    check "renovate.json keeps every Fedora digest pin current" "" "$renovate_problems"
    matches=$(python3 - "$APEX_ROOT" <<'PY'
import json, re, sys
cfg = json.load(open(f"{sys.argv[1]}/renovate.json"))
def hits(line):
    return sum(1 for m in cfg["customManagers"] for rx in m["matchStrings"]
               if re.compile(rx.replace("(?<", "(?P<")).search(line))
d = "@sha256:" + "1" * 64
print(" ".join(str(hits(l)) for l in [
    "ARG BASE=registry.fedoraproject.org/fedora-bootc" + d,
    "ARG REPOS_BASE=registry.fedoraproject.org/fedora" + d,
    "ARG BASE=quay.io/fedora/fedora-bootc" + d,
    "ARG REPOS_BASE=quay.io/fedora/fedora" + d]))
PY
)
    check "managers: registry.fedoraproject.org BASE and REPOS_BASE match one each, quay.io none" \
        "1 1 0 0" "$matches"
fi

# --- ci-host.sh: preflight measures the scratch directory storage chose -----------------
H="$TMP/host"
mkdir -p "$H/store" "$H/scratch" "$H/runner"
printf '%s\n' "$H/scratch" >"$H/runner/apex-scratch"
host_rc=0
host_log=$(env -u APEX_SCRATCH RUNNER_TEMP="$H/runner" APEX_CI_STORE="$H/store" GITHUB_ACTIONS='' \
    "$APEX_ROOT/.github/scripts/ci-host.sh" preflight 0 0 2>&1) || host_rc=$?
check "preflight passes" 0 "$host_rc"
check_true "preflight reads the recorded scratch directory, not /var/tmp" grep -qF -- "$H/scratch" <<<"$host_log"
host_log=$(env -u APEX_SCRATCH RUNNER_TEMP="$H/none" APEX_CI_STORE="$H/store" GITHUB_ACTIONS='' \
    "$APEX_ROOT/.github/scripts/ci-host.sh" preflight 0 0 2>&1 || true)
check_true "preflight without a recorded directory says so" grep -q "no scratch directory recorded" <<<"$host_log"
check "APEX_SCRATCH wins over the record" /elsewhere \
    "$(APEX_SCRATCH=/elsewhere RUNNER_TEMP="$H/runner" bash -c '. "$1"; scratch_dir' _ "$APEX_ROOT/.github/scripts/ci-host.sh")"

# --- qualify.sh: the guests never depend on Flathub -------------------------------------
kargs=$(bash -c '. "$1"; install_kargs' _ "$APEX_ROOT/.github/scripts/qualify.sh" | paste -sd' ' -)
for unit in apex-flatpak-preinstall.service apex-flatpak-preinstall.timer \
    apex-flatpak-update.service apex-flatpak-update.timer; do
    check_true "qualification masks $unit" grep -qF -- "--karg systemd.mask=$unit" <<<"$kargs"
done
check_true "qualification still enables sshd" grep -qF -- "--karg systemd.wants=sshd.service" <<<"$kargs"
units_dir="$APEX_ROOT/image/system_files/usr/lib/systemd/system"
if [[ -d $units_dir ]]; then
    unmasked=$(find "$units_dir" -maxdepth 1 -name '*flatpak*' -printf '%f\n' | sort |
        while read -r u; do grep -qF -- "systemd.mask=$u" <<<"$kargs" || printf '%s ' "$u"; done)
    check "every Flatpak unit of the image is masked in qualification" "" "$unmasked"
fi

# --- image.sh replay-check: only main-branch image.yml runs of this repository ----------
run_json() { # EVENT BRANCH HEAD_REPO [PATH]
    jq -n --arg e "$1" --arg b "$2" --arg h "$3" --arg p "${4:-.github/workflows/image.yml}" \
        '{path: $p, event: $e, head_branch: $b, head_sha: "abc", status: "completed",
          conclusion: "success", head_repository: {full_name: $h},
          repository: {full_name: "SekiroKenjii/apex-fedora"}}'
}
R=SekiroKenjii/apex-fedora
check_true "a scheduled main run may be replayed" replay_run_ok "$R" <<<"$(run_json schedule main "$R")"
check_true "a push to main may be replayed" replay_run_ok "$R" <<<"$(run_json push main "$R")"
check_false "a pull request run may not" replay_run_ok "$R" <<<"$(run_json pull_request main "$R")"
check_false "a fork's run may not" replay_run_ok "$R" <<<"$(run_json pull_request main evil/apex-fedora)"
check_false "a dispatch on another branch may not" replay_run_ok "$R" <<<"$(run_json workflow_dispatch wip "$R")"
check_false "another workflow's run may not" replay_run_ok "$R" \
    <<<"$(run_json push main "$R" .github/workflows/ci.yml)"
check_false "a run still in progress may not" replay_run_ok "$R" \
    <<<"$(run_json push main "$R" | jq '.status = "in_progress"')"
check_false "a run without a head repository may not" replay_run_ok "$R" \
    <<<"$(run_json push main "$R" | jq '.head_repository = null')"
mkdir -p "$TMP/gh-bin" "$TMP/runs"
cat >"$TMP/gh-bin/gh" <<'EOF'
#!/usr/bin/env bash
# Fake gh: `gh api repos/<repo>/actions/runs/<id>` answers from $FAKE_RUNS/<id>.json.
[[ $1 == api ]] || exit 3
id=${2##*/}
cat "$FAKE_RUNS/$id.json" 2>/dev/null || { echo 'HTTP 404' >&2; exit 1; }
EOF
chmod +x "$TMP/gh-bin/gh"
run_json schedule main "$R" >"$TMP/runs/11.json"
run_json pull_request main evil/apex-fedora >"$TMP/runs/12.json"
replay() {
    PATH="$TMP/gh-bin:$PATH" FAKE_RUNS="$TMP/runs" GITHUB_REPOSITORY=$R GITHUB_ACTIONS='' \
        GITHUB_STEP_SUMMARY=/dev/null rc_of "$APEX_ROOT/.github/scripts/image.sh" replay-check "$1"
}
check "replay-check accepts a main schedule run" 0 "$(replay 11)"
check "replay-check refuses a fork PR run" 1 "$(replay 12)"
check "replay-check refuses an unreadable run" 1 "$(replay 13)"
check "replay-check refuses a non-numeric id" 1 "$(replay '11;x')"

# --- commit order: publish.sh never moves a tag back to older code ----------------------
G="$TMP/git"
git init -q "$G"
gitc() { git -C "$G" -c user.name=t -c user.email=t@example.invalid "$@"; }
gitc commit -q --allow-empty -m A
C_A=$(git -C "$G" rev-parse HEAD)
gitc commit -q --allow-empty -m B
C_B=$(git -C "$G" rev-parse HEAD)
gitc checkout -q -b side "$C_A"
gitc commit -q --allow-empty -m S
C_S=$(git -C "$G" rev-parse HEAD)
C_X=$(printf 'f%.0s' {1..40})
desc_rc() { APEX_GIT_ROOT=$G rc_of revision_descends "$1" "$2"; }
check "a descendant descends" 0 "$(desc_rc "$C_A" "$C_B")"
check "the same commit counts" 0 "$(desc_rc "$C_B" "$C_B")"
check "an older commit does not" 1 "$(desc_rc "$C_B" "$C_A")"
check "a sibling branch does not" 1 "$(desc_rc "$C_B" "$C_S")"
check "an unknown commit is unknown, not older" 2 "$(desc_rc "$C_X" "$C_B")"
check "an empty revision is unknown" 2 "$(desc_rc "" "$C_B")"

P="$TMP/publish"
mkdir -p "$P/bin" "$P/reg"
cat >"$P/bin/skopeo" <<'EOF'
#!/usr/bin/env bash
ref=${*: -1}
ref=${ref#docker://}
if [[ $1 == list-tags ]]; then
    [[ -f $FAKE_REG/tags.json ]] || { echo 'Error: listing tags: 502 Bad Gateway' >&2; exit 1; }
    cat "$FAKE_REG/tags.json"
    exit 0
fi
tag=${ref##*:}
if [[ -f $FAKE_REG/$tag.json ]]; then cat "$FAKE_REG/$tag.json"; exit 0; fi
echo "Error: reading manifest $tag in ${ref%:*}: manifest unknown" >&2
exit 1
EOF
chmod +x "$P/bin/skopeo"
label_json() { # DIGEST_CHAR VERSION REVISION
    jq -n --arg d "sha256:$(printf "$1%.0s" {1..64})" --arg v "$2" --arg r "$3" \
        '{Digest: $d, Labels: {"org.opencontainers.image.version": $v, "org.opencontainers.image.revision": $r}}'
}
increases() {
    # shellcheck disable=SC2016 # expanded by the inner bash
    PATH="$P/bin:$PATH" FAKE_REG="$P/reg" APEX_GIT_ROOT="$G" APEX_RETRY_DELAY=0 GITHUB_ACTIONS='' \
        rc_of bash -c '. "$1"; shift; check_increases "$@"' _ "$APEX_ROOT/.github/scripts/publish.sh" "$@"
}
D1="sha256:$(printf '1%.0s' {1..64})"
D2="sha256:$(printf '2%.0s' {1..64})"
label_json 1 45.20260924.150 "$C_A" >"$P/reg/45.json"
check "a newer version on a descendant commit moves" 0 "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_B")"
label_json 1 45.20260924.150 "$C_B" >"$P/reg/45.json"
check "a re-run of an older run (newer date, ancestor commit) is refused" 1 \
    "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.149 "$C_A")"
check "a newer version on a commit off main's line is refused" 1 \
    "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_S")"
check "an unknown candidate commit is refused" 1 "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_X")"
check "already there is a no-op" 10 "$(increases ghcr.io/x/t:45 "$D1" 45.20260925.151 "$C_B")"
check "an older version is refused" 1 "$(increases ghcr.io/x/t:45 "$D2" 45.20260923.999 "$C_B")"
rm -f "$P/reg/45.json"
printf '{"Repository":"ghcr.io/x/t","Tags":["sha-1","45.20260924.150"]}' >"$P/reg/tags.json"
check "an absent tag that list-tags confirms is a first publication" 0 \
    "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_B")"
printf '{"Repository":"ghcr.io/x/t","Tags":["45"]}' >"$P/reg/tags.json"
check "an absent tag that list-tags still lists is refused" 1 \
    "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_B")"
rm -f "$P/reg/tags.json"
check "an absent tag that list-tags cannot confirm is refused" 1 \
    "$(increases ghcr.io/x/t:45 "$D2" 45.20260925.151 "$C_B")"

# --- oci-artifact.sh lookup: a cache hit must be attested by the workflow on main ------
O="$TMP/oci"
mkdir -p "$O/bin"
printf '{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}' >"$O/manifest"
O_DIGEST="sha256:$(sha256sum <"$O/manifest" | cut -d' ' -f1)"
cat >"$O/bin/skopeo" <<'EOF'
#!/usr/bin/env bash
ref=${*: -1}
case $ref in
    *missing*) echo "Error: reading manifest x in y: manifest unknown" >&2; exit 1 ;;
esac
if [[ " $* " == *" --raw "* ]]; then cat "$FAKE_MANIFEST"; exit 0; fi
printf '{"Digest":"%s","Labels":{"io.github.sekirokenjii.apex.content-key":"%s"}}\n' "$FAKE_DIGEST" "$FAKE_LABEL"
EOF
cat >"$O/bin/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$FAKE_GH_LOG"
[[ $FAKE_GH_OK == 1 ]]
EOF
chmod +x "$O/bin/skopeo" "$O/bin/gh"
lookup_rc() {
    local key=$1 label=$2 gh_ok=$3 repo=${4:-ghcr.io/x/apex-rpms}
    PATH="$O/bin:$PATH" FAKE_MANIFEST="$O/manifest" FAKE_DIGEST="$O_DIGEST" FAKE_LABEL=$label \
        FAKE_GH_OK=$gh_ok FAKE_GH_LOG="$O/gh.log" APEX_RETRY_DELAY=0 GITHUB_ACTIONS='' \
        rc_of "$APEX_ROOT/.github/scripts/oci-artifact.sh" lookup "$repo" "$key" rpms.yml
}
: >"$O/gh.log"
check "an attested artifact with its key is a hit" 0 "$(lookup_rc k1 k1 1)"
check_true "gh checks the rpms.yml identity on main" \
    grep -qF -- "--cert-identity https://github.com/SekiroKenjii/apex-fedora/.github/workflows/rpms.yml@refs/heads/main" "$O/gh.log"
check_true "gh checks the source ref and hosted runners" \
    grep -qE -- "--source-ref refs/heads/main .*--deny-self-hosted-runners" "$O/gh.log"
check "an unattested artifact is a miss" 5 "$(lookup_rc k1 k1 0)"
check "an artifact labelled with another key is a miss" 5 "$(lookup_rc k1 k2 1)"
check "an absent artifact is a miss" 3 "$(lookup_rc k1 k1 1 ghcr.io/x/missing)"
check "a bad workflow name is refused" 1 \
    "$(PATH="$O/bin:$PATH" GITHUB_ACTIONS='' rc_of "$APEX_ROOT/.github/scripts/oci-artifact.sh" lookup ghcr.io/x/y k '../evil')"

# --- privacy guard: biometric files, hex dumps and long lines never get tracked ---------
V="$TMP/privacy"
git init -q "$V"
guard_rc() { GITHUB_ACTIONS='' rc_of "$APEX_ROOT/.github/scripts/privacy-guard.sh" --dir "$V" "$@"; }
guard_findings() {
    GITHUB_ACTIONS='' "$APEX_ROOT/.github/scripts/privacy-guard.sh" --dir "$V" 2>&1 |
        grep -c '^privacy guard: [^0-9]' || true
}
tracked_fails() {
    local name=$1 path=$2 content=$3
    mkdir -p "$V/$(dirname "$path")"
    printf '%s\n' "$content" >"$V/$path"
    git -C "$V" add -f "$path"
    check "$name" "1:1" "$(guard_rc):$(guard_findings)"
    git -C "$V" rm -q --cached "$path"
    rm -f "$V/$path"
}
tracked_passes() {
    local name=$1 path=$2 content=$3
    mkdir -p "$V/$(dirname "$path")"
    printf '%s\n' "$content" >"$V/$path"
    git -C "$V" add -f "$path"
    check "$name" 0 "$(guard_rc)"
}
x_times() { printf "${2:-x}%.0s" $(seq "$1"); }
table_row() { printf '| %s |' "$(printf 'word %.0s' $(seq "$1"))"; }

tracked_passes "a clean file passes" README.md hello
tracked_passes "hex bytes with no colon before them pass" short-hex.txt "short hex $(x_times 40 '0a ')"
tracked_passes "a digest line passes" digest.txt "sha256 $(x_times 32 ab)"
tracked_passes "four bytes after a colon pass" usb.log "usb: 0a 1b 2c 3d"
tracked_passes "a hex-dump line outside *.log and *.txt passes that rule" notes.md "usb: 0a 1b 2c 3d 4e"
tracked_passes "a 399-character line passes" wide.conf "$(x_times 399)"
tracked_passes "a long line in Cargo.lock passes" Cargo.lock "$(x_times 500)"
tracked_passes "a long line in a nested Cargo.lock passes" crates/x/Cargo.lock "$(x_times 500)"
tracked_passes "a long line in an insta snapshot passes" crates/x/src/snapshots/x__y.snap "$(x_times 500)"
tracked_passes "a long line in the bootc progress fixtures passes" \
    crates/apex-bootc/tests/fixtures/progress/x.jsonl "{\"a\": \"$(x_times 500)\"}"
tracked_passes "a long Markdown table row of words passes" docs/table.md "$(table_row 100)"
check "all of that together passes" 0 "$(guard_rc)"

printf 'P5\n' >"$V/enrolled.pgm"
check "an untracked image is not tracked yet" 0 "$(guard_rc)"
check "--untracked sees it" 1 "$(guard_rc --untracked)"
rm -f "$V/enrolled.pgm"
for f in enrolled.pgm print.VARIANT rec.ioctl rec.umockdev cap.pcap cap.pcapng var/lib/fprint/user/1 \
    a/var/lib/fprint/b; do
    tracked_fails "a tracked $f fails" "$f" ''
done
tracked_fails "a long line of hex bytes fails" dump.log "fpi-usb DEBUG transfer: $(printf '%02x ' {0..199})"
tracked_fails "a long C array of hex bytes fails" frame.c \
    "static const guint8 frame[] = { $(printf '0x%02x, ' {0..199})};"
tracked_fails "128 hex bytes fail under 400 characters too" frame.h "$(x_times 128 5a)"
tracked_fails "a hex run fails in a file exempt from the length rule" Cargo.lock "x = \"$(x_times 150 'ab ')\""
tracked_fails "a hex-dump line in a .log fails" usb.log "usb: 0a 1b 2c 3d 4e"
tracked_fails "a hex-dump line in a .TXT fails" CAPTURE.TXT "rx: 0a1b2c3d4e5f   "
tracked_fails "a 400-character line fails" wide.conf "$(x_times 400)"
tracked_fails "a long line in a .txt fails" long-text.txt "$(x_times 500)"
tracked_fails "a long base64 line fails" frame.json "{\"frame\": \"$(x_times 120 QUJD)\"}"
tracked_fails "a long Markdown line outside a table fails" docs/prose.md "$(x_times 450 'y ')"
tracked_fails "a Markdown table row with a long blob fails" docs/blob.md "| frame | $(x_times 150 QUJD) |"
tracked_fails "a table row outside Markdown fails" table.txt "$(table_row 100)"
tracked_fails "a long line in another Rust file fails" crates/x/src/lib.rs "$(x_times 450)"
mkdir -p "$V/leak"
printf '%s\n' "SECRETMARKER $(x_times 450)" >"$V/leak/data.txt"
git -C "$V" add leak/data.txt
leak_out=$(GITHUB_ACTIONS='' "$APEX_ROOT/.github/scripts/privacy-guard.sh" --dir "$V" 2>&1 || true)
check_true "the finding names the file and line" grep -q '^privacy guard: leak/data.txt:1: ' <<<"$leak_out"
check_false "the finding does not print the line" grep -q SECRETMARKER <<<"$leak_out"
git -C "$V" rm -q --cached leak/data.txt
check "the tree is clean again" 0 "$(guard_rc)"
check "the repository itself is clean" 0 \
    "$(GITHUB_ACTIONS='' rc_of "$APEX_ROOT/.github/scripts/privacy-guard.sh" --untracked)"
for f in a.pgm B.PGM x/print.Variant rec.ioctl x/y/rec.UMOCKDEV cap.pcap cap.PcapNG \
    var/lib/fprint/u/1 image/system_files/var/lib/fprint/u/1; do
    check_true ".gitignore ignores $f" git -C "$APEX_ROOT" check-ignore -q --no-index "$f"
done
check_false ".gitignore keeps the fingerprint documentation" \
    git -C "$APEX_ROOT" check-ignore -q --no-index docs/hardware/FINGERPRINT.md

if ((FAILS > 0)); then
    die "selftest: $FAILS failure(s)"
fi
log "selftest: all $TESTS checks passed (plus the registry checks)"
