#!/usr/bin/env bash
# Idempotent, and never writes to the codec, reloads a module or rebuilds an initramfs.

set -euo pipefail
shopt -s inherit_errexit nullglob
# Bytes, not locale characters: the kernel's 127-character line limit counts bytes.
export LC_ALL=C

readonly EXIT_USAGE=1 EXIT_REFUSED=2
readonly CODEC_LINE="0x10ec0294 0x10431ab2 0"
readonly FW_NAME=alc294-m7400qc.fw
readonly FW_REF=apex/$FW_NAME
readonly PATCH_SLOTS=4

PROG=${0##*/}
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=${APEX_TRYFIX_ROOT:-}
GENERATED=${APEX_TRYFIX_GENERATED:-$HERE/../generated}
FW_SRC=$GENERATED/$FW_NAME
CONF_SRC=$GENERATED/apex-audio.conf
FW_DIR=$ROOT/lib/firmware/apex
FW_DST=$FW_DIR/$FW_NAME
CONF_DST=$ROOT/etc/modprobe.d/apex-audio.conf
KVER=$(uname -r)

usage() {
    cat <<EOF
Usage: sudo $PROG install|revert|status

install  validate and copy the rendered fix:
           ${FW_SRC#"$HERE/"} -> /lib/firmware/apex/$FW_NAME
           ${CONF_SRC#"$HERE/"} -> /etc/modprobe.d/apex-audio.conf
revert   remove exactly those two files
status   show the installed files and what the running kernel loaded, and compare
         the loaded init_verbs with the rendered [verb] lines

Render the files first with 'just render' (or apexctl profile render), which needs
measured values in hardware/m7400qc/profile.toml. See docs/hardware/AUDIO.md.
EOF
}

die() {
    local code=$1
    shift
    printf '%s: %s\n' "$PROG" "$*" >&2
    exit "$code"
}

say() { printf '%s\n' "$*"; }

require_root() {
    if ((EUID != 0)); then
        if [[ -z $ROOT || ${APEX_TRYFIX_ALLOW_NONROOT:-} != 1 ]]; then
            die "$EXIT_REFUSED" "must run as root: sudo $0 $ACTION"
        fi
    fi
}

EXPECTED_INIT_VERBS=()

# Accepts only what apexctl profile render writes, in the kernel's init_verbs form.
validate_fw() {
    local file=$1 line n=0 section='' codec_seen=0 codec_lines=0 verbs=0 pending_index=0
    local nid verb param canonical
    [[ -f $file && ! -L $file ]] || die "$EXIT_REFUSED" "$file is missing or not a regular file; render it first (just render)"
    (($(stat -c %s -- "$file") <= 65536)) || die "$EXIT_REFUSED" "$file is larger than 64 KiB; that is not a rendered patch file"
    EXPECTED_INIT_VERBS=()
    while IFS= read -r line || [[ -n $line ]]; do
        n=$((n + 1))
        [[ $line != *$'\r'* ]] || die "$EXIT_REFUSED" "$file:$n: carriage return found; the file must use LF line endings"
        ((${#line} <= 127)) || die "$EXIT_REFUSED" "$file:$n: line longer than 127 bytes; the kernel's patch loader stops reading the file there"
        [[ -n $line && $line != \#* ]] || continue
        [[ $line != [[:space:]]* ]] || die "$EXIT_REFUSED" "$file:$n: leading whitespace; apexctl profile render never writes it (render again)"
        if [[ $line == \[* ]]; then
            case $line in
            "[codec]")
                ((codec_seen == 0)) || die "$EXIT_REFUSED" "$file:$n: a second [codec] section"
                codec_seen=1
                section=codec
                ;;
            "[verb]" | "[init_verbs]")
                ((codec_seen == 1)) || die "$EXIT_REFUSED" "$file:$n: [verb] before [codec]; the kernel would ignore it"
                section=verb
                ;;
            *)
                die "$EXIT_REFUSED" "$file:$n: section '$line' is not allowed; ADR 0004 files have only [codec] and [verb]"
                ;;
            esac
            continue
        fi
        case $section in
        codec)
            [[ $line == "$CODEC_LINE" ]] ||
                die "$EXIT_REFUSED" "$file:$n: the [codec] line must be exactly '$CODEC_LINE', found '$line'"
            codec_lines=$((codec_lines + 1))
            ;;
        verb)
            [[ $line =~ ^(0[xX][0-9a-fA-F]+)\ (0[xX][0-9a-fA-F]+)\ (0[xX][0-9a-fA-F]+)$ ]] ||
                die "$EXIT_REFUSED" "$file:$n: expected '0xNID 0xVERB 0xPARAM', found '$line'"
            nid=$((BASH_REMATCH[1]))
            verb=$((BASH_REMATCH[2]))
            param=$((BASH_REMATCH[3]))
            ((nid == 0x20)) || die "$EXIT_REFUSED" "$file:$n: only NID 0x20 (the COEF widget) is allowed"
            ((param <= 0xffff)) || die "$EXIT_REFUSED" "$file:$n: parameter larger than 16 bits"
            canonical=$(printf '0x%02x 0x%03x 0x%04x' "$nid" "$verb" "$param")
            [[ $line == "$canonical" ]] ||
                die "$EXIT_REFUSED" "$file:$n: '$line' is not in the renderer's init_verbs form '$canonical'; render the file again (just render)"
            case $verb in
            $((0x500)))
                pending_index=1
                ;;
            $((0x400)))
                ((pending_index == 1)) || die "$EXIT_REFUSED" "$file:$n: a COEF write (0x400) without a preceding index (0x500)"
                pending_index=0
                ;;
            *)
                # 0x477-style verbs corrupt the value (docs/hardware/AUDIO.md).
                die "$EXIT_REFUSED" "$file:$n: verb $(printf '0x%x' "$verb") is not allowed; use 0x500 (index) and 0x400 (value)"
                ;;
            esac
            verbs=$((verbs + 1))
            EXPECTED_INIT_VERBS+=("$canonical")
            ;;
        *)
            die "$EXIT_REFUSED" "$file:$n: data line outside a section"
            ;;
        esac
    done <"$file"
    ((codec_lines == 1)) || die "$EXIT_REFUSED" "$file: expected exactly one [codec] line '$CODEC_LINE', found $codec_lines"
    ((verbs > 0)) || die "$EXIT_REFUSED" "$file: no [verb] lines; nothing to apply"
    ((pending_index == 0)) || die "$EXIT_REFUSED" "$file: ends with a COEF index but no value"
}

validate_conf() {
    local file=$1 line n=0 options=0 entries entry count
    [[ -f $file && ! -L $file ]] || die "$EXIT_REFUSED" "$file is missing or not a regular file; render it first (just render)"
    while IFS= read -r line || [[ -n $line ]]; do
        n=$((n + 1))
        [[ -n ${line//[[:space:]]/} && $line != \#* ]] || continue
        [[ $line =~ ^options[[:space:]]+snd[-_]hda[-_]intel[[:space:]]+patch=([^[:space:]]+)[[:space:]]*$ ]] ||
            die "$EXIT_REFUSED" "$file:$n: expected 'options snd-hda-intel patch=...', found '$line'"
        options=$((options + 1))
        entries=${BASH_REMATCH[1]}
        IFS=, read -r -a parts <<<"$entries"
        count=${#parts[@]}
        ((count == PATCH_SLOTS)) ||
            die "$EXIT_REFUSED" "$file:$n: $count patch= entries; the contract (ADR 0004) says $PATCH_SLOTS identical entries"
        for entry in "${parts[@]}"; do
            [[ $entry == "$FW_REF" ]] || die "$EXIT_REFUSED" "$file:$n: patch= entry '$entry' is not '$FW_REF'"
        done
    done <"$file"
    ((options == 1)) || die "$EXIT_REFUSED" "$file: expected exactly one options line, found $options"
}

check_patch_loader() {
    local config=$ROOT/boot/config-$KVER value=
    if [[ -r $config ]]; then
        value=$(grep -E '^CONFIG_SND_HDA_PATCH_LOADER=' "$config" || true)
    elif [[ -r $ROOT/proc/config.gz ]]; then
        value=$(zcat -- "$ROOT/proc/config.gz" | grep -E '^CONFIG_SND_HDA_PATCH_LOADER=' || true)
    else
        die "$EXIT_REFUSED" "cannot find the kernel config for $KVER (/boot/config-$KVER or /proc/config.gz)"
    fi
    [[ $value == CONFIG_SND_HDA_PATCH_LOADER=y ]] ||
        die "$EXIT_REFUSED" "kernel $KVER does not have CONFIG_SND_HDA_PATCH_LOADER=y ('${value:-unset}'); the patch file would be ignored"
}

# Other modprobe.d files that also set snd-hda-intel patch= would fight with ours.
other_patch_options() {
    local f
    for f in "$ROOT"/etc/modprobe.d/*.conf "$ROOT"/run/modprobe.d/*.conf \
        "$ROOT"/usr/local/lib/modprobe.d/*.conf "$ROOT"/usr/lib/modprobe.d/*.conf "$ROOT"/lib/modprobe.d/*.conf; do
        [[ $f -ef $CONF_DST ]] && continue
        if grep -Eqs '^[[:space:]]*options[[:space:]]+snd[-_]hda[-_]intel[[:space:]].*\bpatch=' "$f"; then
            printf '%s\n' "${f#"$ROOT"}"
        fi
    done | sort -u
}

initramfs_note() {
    local initrd=$ROOT/boot/initrd.img-$KVER listing
    if command -v lsinitramfs >/dev/null && [[ -r $initrd ]]; then
        listing=$(lsinitramfs "$initrd") || die "$EXIT_REFUSED" "lsinitramfs $initrd failed"
        if grep -Eq '(^|/)snd-hda-intel\.ko(\.[a-z0-9]+)?$' <<<"$listing"; then
            say ""
            say "NOTE: snd-hda-intel is inside the initramfs ${initrd#"$ROOT"}."
            say "  Run this yourself (not done automatically):  sudo update-initramfs -u"
            say "  The initramfs must also contain /lib/firmware/$FW_REF, or the driver logs"
            say "  'Cannot load firmware, continue without patching'. See the initramfs section"
            say "  of docs/hardware/AUDIO.md for the hook that adds it."
        else
            say "initramfs: snd-hda-intel is not in ${initrd#"$ROOT"}; no initramfs update needed."
        fi
    else
        say "initramfs: could not inspect ${initrd#"$ROOT"} (lsinitramfs missing or file unreadable);"
        say "  check yourself: lsinitramfs /boot/initrd.img-\$(uname -r) | grep snd-hda-intel"
    fi
}

copy_atomic() {
    local src=$1 dst=$2 tmp
    [[ ! -L $dst ]] || die "$EXIT_REFUSED" "$dst is a symlink; refusing to replace it"
    if [[ -f $dst ]] && cmp -s -- "$src" "$dst"; then
        say "unchanged  ${dst#"$ROOT"}"
        return 0
    fi
    tmp=$(mktemp -- "$(dirname -- "$dst")/.apex-tryfix.XXXXXX")
    cat -- "$src" >"$tmp"
    chmod 0644 -- "$tmp"
    ((EUID != 0)) || chown root:root -- "$tmp"
    if [[ -e $dst ]]; then
        mv -f -- "$tmp" "$dst"
        say "replaced   ${dst#"$ROOT"}"
    else
        mv -f -- "$tmp" "$dst"
        say "installed  ${dst#"$ROOT"}"
    fi
}

# apexctl writes [verb] lines in the kernel's init_verbs format, so they compare as text.
rendered_verbs() {
    sed -n '/^\[verb\]$/,/^\[/{/^0x/p}' -- "$1"
}

codec_init_verbs() {
    local dir
    for dir in "$ROOT"/sys/class/sound/hwC*D*; do
        [[ $(cat -- "$dir/vendor_id" 2>/dev/null) == 0x10ec0294 &&
            $(cat -- "$dir/subsystem_id" 2>/dev/null) == 0x10431ab2 ]] || continue
        printf '%s\n' "${dir#"$ROOT"}"
        cat -- "$dir/init_verbs" 2>/dev/null || true
        return 0
    done
    return 1
}

print_verify() {
    say ""
    say "Verify after the reboot:"
    say "  sudo $0 status      # compares the ALC294's init_verbs with the rendered file; expected:"
    local v
    for v in "${EXPECTED_INIT_VERBS[@]}"; do say "      $v"; done
    say "  sudo journalctl -k -b | grep -E 'Applying patch firmware|Cannot load firmware'"
    say "      expected: Applying patch firmware '$FW_REF' and no 'Cannot load firmware'"
    say "Then record the COEFs with coef-dump.sh for the phase (C, D or E)."
}

do_install() {
    require_root
    validate_fw "$FW_SRC"
    validate_conf "$CONF_SRC"
    check_patch_loader
    local others
    others=$(other_patch_options)
    [[ -z $others ]] || die "$EXIT_REFUSED" "other modprobe.d files already set snd-hda-intel patch=: ${others//$'\n'/, }; remove that first"
    install -d -m 0755 -- "$FW_DIR" "$(dirname -- "$CONF_DST")"
    copy_atomic "$FW_SRC" "$FW_DST"
    copy_atomic "$CONF_SRC" "$CONF_DST"
    initramfs_note
    say ""
    say "Next: the patch file is read when snd-hda-intel probes, so reboot. Pick the phase:"
    say "  C  boot Windows, play sound on the speakers, choose Restart, boot Linux,"
    say "     then: sudo $HERE/coef-dump.sh C   (then D: suspend, resume, coef-dump.sh D)"
    say "  E  full power-off: shut down, unplug the AC adapter, wait 30 s, power on,"
    say "     then: sudo $HERE/coef-dump.sh E"
    print_verify
}

do_revert() {
    require_root
    local f removed=0
    for f in "$FW_DST" "$CONF_DST"; do
        if [[ -e $f || -L $f ]]; then
            rm -f -- "$f"
            say "removed    ${f#"$ROOT"}"
            removed=1
        else
            say "absent     ${f#"$ROOT"}"
        fi
    done
    ((removed == 0)) || initramfs_note
    say ""
    say "Next: the codec keeps the COEF values the fix wrote until it loses power, so a"
    say "reboot alone is not a clean 'without fix' state. For phases A and B, shut down,"
    say "unplug the AC adapter, wait 30 s and power on (A), or go through Windows (B)."
    say "Then check that the fix is gone:"
    say "  cat /sys/class/sound/hwC*D0/init_verbs      # expected: empty"
    say "The directory ${FW_DIR#"$ROOT"} is left in place (rmdir it if you like)."
}

do_status() {
    local f out
    for f in "$FW_DST" "$CONF_DST"; do
        if [[ -f $f ]]; then say "present    ${f#"$ROOT"}"; else say "absent     ${f#"$ROOT"}"; fi
    done
    if [[ -f $CONF_DST ]] && [[ -f $CONF_SRC ]] && ! cmp -s -- "$CONF_SRC" "$CONF_DST"; then
        say "note: the installed apex-audio.conf differs from ${CONF_SRC#"$HERE/"}"
    fi
    if [[ -f $FW_DST ]] && [[ -f $FW_SRC ]] && ! cmp -s -- "$FW_SRC" "$FW_DST"; then
        say "note: the installed $FW_NAME differs from ${FW_SRC#"$HERE/"} (install again)"
    fi
    say "snd_hda_intel patch= (running): $(sed -E 's/(,\(null\))+$//' "$ROOT/sys/module/snd_hda_intel/parameters/patch" 2>/dev/null || echo unknown)"
    if out=$(codec_init_verbs); then
        say "init_verbs of ${out%%$'\n'*}:"
        if [[ $out == *$'\n'* && -n ${out#*$'\n'} ]]; then
            local loaded=${out#*$'\n'}
            while IFS= read -r f; do say "  $f"; done <<<"$loaded"
            if [[ ! -f $FW_SRC ]]; then
                say "  (no rendered ${FW_SRC#"$HERE/"} to compare with)"
            elif [[ $(rendered_verbs "$FW_SRC") == "${loaded%$'\n'}" ]]; then
                say "  match: equal to the [verb] lines of ${FW_SRC#"$HERE/"}"
            else
                say "  MISMATCH: not the [verb] lines of ${FW_SRC#"$HERE/"} (install again and reboot)"
            fi
        else
            say "  (empty: no fix loaded in the running kernel)"
        fi
    else
        say "no ALC294 (subsystem 0x10431ab2) hwdep found under /sys/class/sound"
    fi
}

ACTION=${1:-}
(($# <= 1)) || die "$EXIT_USAGE" "exactly one action expected (see --help)"
case $ACTION in
install) do_install ;;
revert) do_revert ;;
status) do_status ;;
-h | --help)
    usage
    ;;
*)
    usage >&2
    exit "$EXIT_USAGE"
    ;;
esac
