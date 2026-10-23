#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Against fakes only: no network, no root, no KVM.

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require awk sed mktemp git

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

rc_of() { local rc=0; "$@" >/dev/null 2>&1 || rc=$?; printf '%s' "$rc"; }

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
check "the recipe starts with Fedora's repositories" \
    "repos --repo=fedora --repo=updates" \
    "$(bash -c '. "$1"; git_() { printf "100755 blob 1111\t.github/scripts/rpms.sh\n"; }; recipe' _ "$APEX_ROOT/.github/scripts/rpms.sh" | head -n 1)"
check "the recipe covers rpms.sh" ".github/scripts/rpms.sh" \
    "$(bash -c '. "$1"; printf "%s\n" "${RECIPE_PATHS[*]}"' _ "$APEX_ROOT/.github/scripts/rpms.sh")"
check "the key covers the Makefile, Cargo.toml and the packaged docs" \
    "crates packaging/apex.spec Makefile Cargo.lock Cargo.toml LICENSE README.md" \
    "$(bash -c '. "$1"; printf "%s\n" "${KEY_PATHS[*]}"' _ "$APEX_ROOT/.github/scripts/rpms.sh")"
atree=$(bash -c '. "$1"; git_() { [[ $1 != ls-tree ]] || printf "100644 blob 1111\tcrates/apexctl/src/main.rs\n100644 blob 2222\tcrates/apex-boot/src/lib.rs\n"; }; apex_tree' _ "$APEX_ROOT/.github/scripts/rpms.sh" 2>/dev/null)
check "the apex key covers every crate of the workspace" "100644 blob 1111	crates/apexctl/src/main.rs
100644 blob 2222	crates/apex-boot/src/lib.rs" "$atree"

# --- the installed layout: M1-IMPLEMENTATION.md, make install and apex.spec agree ---------
if command -v make >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    L="$TMP/layout"
    mkdir -p "$L/target/release"
    for b in apexctl apex-updated apex-boot-assess apex-session-agent; do
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
          "_userunitdir": "/usr/lib/systemd/user", "_userpresetdir": "/usr/lib/systemd/user-preset",
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


# --- .gitignore: biometric data never gets tracked --------------------------------------
for f in a.pgm B.PGM x/print.Variant rec.ioctl x/y/rec.UMOCKDEV cap.pcap cap.PcapNG var/lib/fprint/u/1; do
    check_true ".gitignore ignores $f" git -C "$APEX_ROOT" check-ignore -q --no-index "$f"
done
check_false ".gitignore keeps the hardware profile" \
    git -C "$APEX_ROOT" check-ignore -q --no-index hardware/m7400qc/profile.toml

if ((FAILS > 0)); then
    die "selftest: $FAILS failure(s)"
fi
log "selftest: all $TESTS checks passed"
