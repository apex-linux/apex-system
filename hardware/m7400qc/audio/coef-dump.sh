#!/usr/bin/env bash

set -euo pipefail
# Without inherit_errexit a failed read inside $(...) would pass as a value of 0.
shopt -s inherit_errexit nullglob
# Bytes: lengths, [[:print:]] and sort order must mean what apexctl and the kernel mean.
export LC_ALL=C

readonly EXIT_USAGE=1 EXIT_REFUSED=2 EXIT_READ_FAILED=3 EXIT_INTERNAL=70
readonly VENDOR_ID=0x10ec0294
readonly SUBSYSTEM_ID=0x10431ab2
readonly COEF_NID=0x20
readonly DEFAULT_INDEXES=(0x0f 0x1b 0x40)
readonly COEF_1B_FIX_BIT=0x0400
# apexctl's limit for [audio.measurements] bios and kernel (profile.rs MAX_PROVENANCE_LEN).
readonly MAX_PROVENANCE_LEN=64
readonly FW_NAME=alc294-m7400qc.fw

SYSROOT=${APEX_AUDIO_SYSROOT:-}
OUTDIR=${APEX_AUDIO_OUTDIR:-/var/tmp/apex-audio}
PROG=${0##*/}
case ${BASH_SOURCE[0]} in
*/*) HERE=${BASH_SOURCE[0]%/*} ;;
*) HERE=. ;;
esac
HERE=$(cd -- "$HERE" && pwd)
GENERATED=${APEX_AUDIO_GENERATED:-$(cd -- "$HERE/.." && pwd)/generated}
RENDERED_FW=$GENERATED/$FW_NAME

usage() {
    cat <<EOF
Usage: sudo $PROG [--extra IDX[,IDX...]]... [--force] PHASE

Reads COEFs 0x0f, 0x1b and 0x40 (plus any --extra indexes, 0x00-0xff) of NID 0x20
on the ALC294 (vendor $VENDOR_ID, subsystem $SUBSYSTEM_ID), prints a summary and a
TOML fragment for hardware/m7400qc/profile.toml, and saves both to
$OUTDIR/<PHASE>-<UTC timestamp>.txt (mode 0644). Nothing is written to the codec.

PHASE is one of (full procedure: docs/hardware/AUDIO.md):
  A  Cold boot WITHOUT the fix. Shut down (not restart), unplug the AC adapter, wait
     30 s, power on straight into Linux. No Windows since the power-off.
     -> 'cold_boot' in [audio.measurements]
  B  Warm reboot from Windows WITHOUT the fix. Boot Windows, log in, play sound
     through the speakers, choose Restart, boot straight into Linux. Check first
     that the speakers are silent or distorted.
     -> 'after_windows'
  C  Fix LOADED after the same Windows warm reboot as B (install the fix with
     try-fix.sh first). This is the test that matters: the fix must repair the
     after-Windows state without a power-off.
     -> 'with_fix_loaded' (informative)
  D  Fix loaded, after a suspend and resume following C, in the same boot
     (systemctl suspend, wake the laptop, then run this).
     -> 'with_fix_after_suspend' (informative)
  E  Cold boot WITH the fix: power-off as in A, then boot Linux. Checks that the
     fix does not break a cold boot (the UX461FN regression).
     -> 'with_fix_cold_boot' (informative)

Options:
  --extra IDX[,IDX...]  also read these COEF indexes (hex, e.g. 0x2a,0x3a)
  --force               skip the phase consistency checks (fix loaded or not,
                        loaded fix equal to the rendered file, suspend count);
                        the other checks always apply
  -h, --help            show this help

Refuses to run unless root, unless exactly one ALC294 with subsystem
$SUBSYSTEM_ID exists, and while any PCM stream of that card is running.
For C, D and E it also refuses when the loaded init_verbs differ from the [verb]
lines of the rendered file (unless --force):
  $RENDERED_FW
EOF
}

die() {
    local code=$1
    shift
    printf '%s: %s\n' "$PROG" "$*" >&2
    exit "$code"
}

PHASE=
FORCE=0
EXTRA=()
while (($# > 0)); do
    case $1 in
    -h | --help)
        usage
        exit 0
        ;;
    --force)
        FORCE=1
        shift
        ;;
    --extra)
        (($# >= 2)) || die "$EXIT_USAGE" "--extra needs a value"
        IFS=, read -r -a parts <<<"$2"
        EXTRA+=("${parts[@]}")
        shift 2
        ;;
    --extra=*)
        IFS=, read -r -a parts <<<"${1#--extra=}"
        EXTRA+=("${parts[@]}")
        shift
        ;;
    -*)
        die "$EXIT_USAGE" "unknown option: $1 (see --help)"
        ;;
    *)
        [[ -z $PHASE ]] || die "$EXIT_USAGE" "only one PHASE may be given"
        PHASE=$1
        shift
        ;;
    esac
done
[[ -n $PHASE ]] || {
    usage >&2
    exit "$EXIT_USAGE"
}

case $PHASE in
A)
    KEY=cold_boot
    PHASE_DESC="cold boot, fix NOT loaded"
    WINDOWS_BEFORE="no (phase A: power-off, then Linux)"
    FIX_EXPECTED=no
    ;;
B)
    KEY=after_windows
    PHASE_DESC="warm reboot from Windows, fix NOT loaded"
    WINDOWS_BEFORE="yes (phase B: Windows, then Restart)"
    FIX_EXPECTED=no
    ;;
C)
    KEY=with_fix_loaded
    PHASE_DESC="warm reboot from Windows, fix loaded"
    WINDOWS_BEFORE="yes (phase C: Windows, then Restart)"
    FIX_EXPECTED=yes
    ;;
D)
    KEY=with_fix_after_suspend
    PHASE_DESC="fix loaded, after suspend and resume (follows C)"
    WINDOWS_BEFORE="yes (phase D follows C in the same boot)"
    FIX_EXPECTED=yes
    ;;
E)
    KEY=with_fix_cold_boot
    PHASE_DESC="cold boot, fix loaded"
    WINDOWS_BEFORE="no (phase E: power-off, then Linux)"
    FIX_EXPECTED=yes
    ;;
*)
    die "$EXIT_USAGE" "PHASE must be one of A B C D E, not '$PHASE' (see --help)"
    ;;
esac

declare -A SEEN=()
INDEXES=()
for raw in "${DEFAULT_INDEXES[@]}" "${EXTRA[@]}"; do
    raw=${raw//[[:space:]]/}
    [[ -n $raw ]] || continue
    [[ $raw =~ ^0[xX][0-9a-fA-F]{1,2}$ ]] || die "$EXIT_USAGE" "COEF index '$raw' is not a hex value 0x00-0xff"
    norm=$(printf '0x%02x' "$((raw))")
    [[ -z ${SEEN[$norm]:-} ]] || continue
    SEEN[$norm]=1
    INDEXES+=("$norm")
done
mapfile -t INDEXES < <(printf '%s\n' "${INDEXES[@]}" | sort)

if ((EUID != 0)); then
    if [[ -z $SYSROOT || ${APEX_AUDIO_ALLOW_NONROOT:-} != 1 ]]; then
        die "$EXIT_REFUSED" "must run as root (the hwdep interface needs CAP_SYS_RAWIO): sudo $0 $PHASE"
    fi
fi

HDA_VERB=$(command -v hda-verb) || die "$EXIT_REFUSED" "hda-verb not found; install alsa-tools"

read_file() {
    local content
    content=$(cat -- "$1" 2>/dev/null) || content=
    content=${content#"${content%%[![:space:]]*}"}
    content=${content%"${content##*[![:space:]]}"}
    printf '%s' "$content"
}

one_line() {
    local text=$1
    if [[ -z $text ]]; then
        printf '(empty)'
    else
        printf '%s' "${text//$'\n'/ | }"
    fi
}

printable() {
    # Keeps a value from breaking a TOML comment or string in the report.
    printf '%s' "$1" | tr -c ' -~' '?'
}

toml_string() {
    local s
    s=$(printable "$1")
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '"%s"' "$s"
}

provenance_ok() {
    local v=$1
    ((${#v} >= 1 && ${#v} <= MAX_PROVENANCE_LEN)) && [[ $v != *[![:print:]]* ]]
}

rendered_verbs() {
    # apexctl writes [verb] lines in the kernel's init_verbs format, so they compare as text.
    sed -n '/^\[verb\]$/,/^\[/{/^0x/p}' -- "$1"
}

CODEC_SYSFS=
MATCHES=()
for dir in "$SYSROOT"/sys/class/sound/hwC*D*; do
    name=${dir##*/}
    [[ $name =~ ^hwC([0-9]+)D([0-9]+)$ ]] || continue
    vendor=$(read_file "$dir/vendor_id")
    subsys=$(read_file "$dir/subsystem_id")
    [[ -n $vendor && -n $subsys ]] || continue
    [[ $vendor =~ ^0x[0-9a-fA-F]+$ && $subsys =~ ^0x[0-9a-fA-F]+$ ]] || continue
    if ((vendor == VENDOR_ID && subsys == SUBSYSTEM_ID)); then
        MATCHES+=("$dir")
    fi
done
((${#MATCHES[@]} > 0)) || die "$EXIT_REFUSED" "no HDA codec with vendor_id $VENDOR_ID and subsystem_id $SUBSYSTEM_ID under /sys/class/sound; is this the M7400QC with snd-hda-intel loaded?"
((${#MATCHES[@]} == 1)) || die "$EXIT_REFUSED" "more than one matching codec (${MATCHES[*]##*/}); refusing to guess"
CODEC_SYSFS=${MATCHES[0]}
HWDEP=${CODEC_SYSFS##*/}
[[ $HWDEP =~ ^hwC([0-9]+)D([0-9]+)$ ]]
CARD=${BASH_REMATCH[1]}
ADDR=${BASH_REMATCH[2]}
DEV=$SYSROOT/dev/snd/$HWDEP
[[ -c $DEV ]] || die "$EXIT_REFUSED" "$DEV is not a character device"

# Refuse while audio is streaming on this card: selecting a COEF slot could race with
# the driver, and the result would not describe an idle codec anyway.
BUSY=()
STATUS_FILES=("$SYSROOT"/proc/asound/card"$CARD"/pcm*/sub*/status)
((${#STATUS_FILES[@]} > 0)) ||
    die "$EXIT_REFUSED" "no /proc/asound/card$CARD/pcm*/sub*/status files (CONFIG_SND_VERBOSE_PROCFS?); cannot confirm the card is idle"
for status in "${STATUS_FILES[@]}"; do
    if grep -Eq '^state:[[:space:]]*(RUNNING|DRAINING)' "$status"; then
        BUSY+=("${status#"$SYSROOT"}")
    fi
done
((${#BUSY[@]} == 0)) || die "$EXIT_REFUSED" "PCM streams are running on card $CARD (${BUSY[*]}); stop playback and recording (or stop PipeWire) and retry"

CHIP=$(read_file "$CODEC_SYSFS/chip_name")
REVISION=$(read_file "$CODEC_SYSFS/revision_id")
MODELNAME=$(read_file "$CODEC_SYSFS/modelname")
INIT_VERBS=$(read_file "$CODEC_SYSFS/init_verbs")
INIT_PINS=$(read_file "$CODEC_SYSFS/init_pin_configs")
DRIVER_PINS=$(read_file "$CODEC_SYSFS/driver_pin_configs")
USER_PINS=$(read_file "$CODEC_SYSFS/user_pin_configs")
HINTS=$(read_file "$CODEC_SYSFS/hints")
module_param() {
    # Drops the trailing "(null)" slots of array parameters.
    local value
    value=$(read_file "$SYSROOT/sys/module/snd_hda_intel/parameters/$1")
    value=$(sed -E 's/(,\(null\))+$//' <<<"$value")
    [[ -n $value && $value != "(null)" ]] || value="(unset)"
    printf '%s' "$value"
}
P_PATCH=$(module_param patch)
P_MODEL=$(module_param model)
P_POWER_SAVE=$(module_param power_save)
KERNEL=$(uname -r)
BIOS=$(read_file "$SYSROOT/sys/class/dmi/id/bios_version")
BOOT_ID=$(read_file "$SYSROOT/proc/sys/kernel/random/boot_id")
UPTIME=$(read_file "$SYSROOT/proc/uptime")
UPTIME=${UPTIME%% *}
SUSPENDS=$(read_file "$SYSROOT/sys/power/suspend_stats/success")

if [[ -n $INIT_VERBS ]]; then
    FIX_LOADED=yes
else
    FIX_LOADED=no
fi

if [[ $FIX_LOADED == no ]]; then
    FIX_MATCH="n/a (no fix loaded)"
elif [[ ! -f $RENDERED_FW ]]; then
    FIX_MATCH="unknown (no rendered $RENDERED_FW to compare with)"
elif [[ $(rendered_verbs "$RENDERED_FW") == "$INIT_VERBS" ]]; then
    FIX_MATCH="init_verbs equal the [verb] lines of $RENDERED_FW"
else
    FIX_MATCH="DIFFERENT: init_verbs are not the [verb] lines of $RENDERED_FW"
fi

# A measurement taken in the wrong fix state would put wrong numbers into profile.toml.
if ((FORCE == 0)); then
    if [[ $FIX_EXPECTED == no && $FIX_LOADED == yes ]]; then
        die "$EXIT_REFUSED" "phase $PHASE is measured WITHOUT the fix, but init_verbs is not empty (fix loaded). Run 'sudo try-fix.sh revert', redo the phase preparation, or pass --force"
    fi
    if [[ $FIX_EXPECTED == yes && $FIX_LOADED == no ]]; then
        die "$EXIT_REFUSED" "phase $PHASE needs the fix loaded, but init_verbs is empty. Run 'sudo try-fix.sh install' and redo the phase preparation, or pass --force"
    fi
    if [[ $FIX_EXPECTED == yes && $FIX_MATCH == DIFFERENT* ]]; then
        die "$EXIT_REFUSED" "phase $PHASE measures the rendered fix, but the loaded init_verbs differ from the [verb] lines of $RENDERED_FW (rendered again after installing?). Run 'sudo try-fix.sh install', redo the phase preparation, or pass --force"
    fi
    if [[ $PHASE == D ]] && ! [[ $SUSPENDS =~ ^[0-9]+$ && $SUSPENDS -ge 1 ]]; then
        die "$EXIT_REFUSED" "phase D needs a completed suspend in this boot, but /sys/power/suspend_stats/success is '${SUSPENDS:-missing}'. Suspend and resume first, or pass --force"
    fi
fi

TMP=$(mktemp -d)
cleanup() { rm -rf -- "$TMP"; }
trap cleanup EXIT

# Read-only: SET_COEF_INDEX only selects a slot, as the kernel's dump_coef does.
run_verb() {
    local verb=$1 param=$2 expect rc=0 out err line value=
    case $verb in
    SET_COEF_INDEX) expect=0x500 ;;
    GET_PROC_COEF) expect=0xc00 ;;
    GET_COEF_INDEX) expect=0xd00 ;;
    *) die "$EXIT_INTERNAL" "internal error: verb '$verb' is not allowed" ;;
    esac
    out=$("$HDA_VERB" "$DEV" "$COEF_NID" "$verb" "$param" 2>"$TMP/stderr") || rc=$?
    err=$(<"$TMP/stderr")
    ((rc == 0)) || die "$EXIT_READ_FAILED" "hda-verb $verb $param exited with $rc: ${err//$'\n'/ }"
    # hda-verb reports a failed ioctl with perror("ioctl") but still prints
    # "value = 0x0" and exits 0, so stderr must be checked.
    [[ $err != *ioctl* && $err != *"Looks like an invalid hwdep"* ]] ||
        die "$EXIT_READ_FAILED" "hda-verb $verb $param failed: ${err//$'\n'/ }"
    # Trust the value only if hda-verb resolved the symbolic name to the expected verb.
    [[ $err =~ nid\ =\ (0x[0-9a-fA-F]+),\ verb\ =\ (0x[0-9a-fA-F]+),\ param\ =\ (0x[0-9a-fA-F]+) ]] ||
        die "$EXIT_READ_FAILED" "unexpected hda-verb stderr for $verb: ${err//$'\n'/ }"
    ((BASH_REMATCH[1] == COEF_NID && BASH_REMATCH[2] == expect && BASH_REMATCH[3] == param)) ||
        die "$EXIT_READ_FAILED" "hda-verb sent nid ${BASH_REMATCH[1]} verb ${BASH_REMATCH[2]} param ${BASH_REMATCH[3]}, expected $COEF_NID $expect $param"
    local count=0
    while IFS= read -r line; do
        line=${line%$'\r'}
        if [[ $line =~ ^value\ =\ 0x([0-9a-fA-F]{1,8})$ ]]; then
            value=$((16#${BASH_REMATCH[1]}))
            count=$((count + 1))
        elif [[ -n ${line//[[:space:]]/} ]]; then
            die "$EXIT_READ_FAILED" "unexpected hda-verb output for $verb: $line"
        fi
    done <<<"$out"
    ((count == 1)) || die "$EXIT_READ_FAILED" "expected one 'value = 0x...' line from hda-verb $verb, got $count"
    printf '%d' "$value"
}

read_coef() {
    local idx=$1 attempt first second index
    for attempt in 1 2 3; do
        first=
        second=
        run_verb SET_COEF_INDEX "$idx" >/dev/null
        index=$(run_verb GET_COEF_INDEX 0)
        ((index == idx)) || continue
        first=$(run_verb GET_PROC_COEF 0)
        run_verb SET_COEF_INDEX "$idx" >/dev/null
        index=$(run_verb GET_COEF_INDEX 0)
        ((index == idx)) || continue
        second=$(run_verb GET_PROC_COEF 0)
        if ((first == second)); then
            if ((first > 0xffff)); then
                die "$EXIT_READ_FAILED" "COEF $idx read back as $(printf '0x%x' "$first"): the codec did not answer"
            fi
            printf '%d' "$first"
            return 0
        fi
        printf '%s: COEF %s changed between two reads (attempt %d), retrying\n' "$PROG" "$idx" "$attempt" >&2
    done
    die "$EXIT_READ_FAILED" "could not get a stable read of COEF $idx after 3 attempts"
}

OLD_INDEX=$(run_verb GET_COEF_INDEX 0)
declare -A VALUE=()
for idx in "${INDEXES[@]}"; do
    VALUE[$idx]=$(read_coef "$idx")
done
# Leave the COEF index as found, as the kernel's proc dumper does.
if ((OLD_INDEX <= 0xffff)); then
    run_verb SET_COEF_INDEX "$(printf '0x%x' "$OLD_INDEX")" >/dev/null
fi

hex4() { printf '0x%04x' "$1"; }

bits_of() {
    local mask=$1 bit out=()
    for ((bit = 0; bit < 16; bit++)); do
        ((mask & (1 << bit))) && out+=("$bit")
    done
    printf '%s' "${out[*]}"
}

# The newest phase-A report (names sort by UTC stamp) is the reference for B-E.
REF_FILE=
declare -A REF=()
if [[ $PHASE != A ]]; then
    a_files=("$OUTDIR"/A-*.txt)
    if ((${#a_files[@]} > 0)); then
        REF_FILE=${a_files[-1]}
        while IFS= read -r pair; do
            [[ $pair =~ index\ =\ (0x[0-9a-fA-F]+),\ value\ =\ (0x[0-9a-fA-F]+) ]] || continue
            REF[$(printf '0x%02x' "$((BASH_REMATCH[1]))")]=$((BASH_REMATCH[2]))
        done < <(grep -E '^cold_boot = \[' "$REF_FILE" | grep -oE 'index = 0x[0-9a-fA-F]+, value = 0x[0-9a-fA-F]+')
    fi
fi

NOW_ISO=$(date +%Y-%m-%dT%H:%M:%S%:z)
TODAY=$(date +%Y-%m-%d)
STAMP=$(date -u +%Y%m%dT%H%M%SZ)

KLOG="(not collected)"
if [[ -z $SYSROOT ]] && command -v journalctl >/dev/null; then
    KLOG=$(journalctl -k -b --no-pager -o short-monotonic 2>/dev/null |
        grep -E 'Applying patch firmware|Cannot load firmware|Direct firmware load for .*alc294|picked fixup|autoconfig for ALC294' |
        tail -n 10) || KLOG=
    [[ -n $KLOG ]] || KLOG="(no matching lines)"
fi

report() {
    local idx v note ref x
    printf '# Apex ALC294 COEF snapshot (hardware/m7400qc/audio/coef-dump.sh)\n'
    printf '# The whole file is valid TOML; everything except the fragment is a comment.\n'
    printf '#\n'
    printf '# phase           : %s (%s)\n' "$PHASE" "$PHASE_DESC"
    printf '# windows before  : %s\n' "$WINDOWS_BEFORE"
    printf '# taken           : %s (uptime %s s)\n' "$NOW_ISO" "${UPTIME:-?}"
    printf '# kernel          : %s\n' "$(printable "$KERNEL")"
    printf '# bios            : %s\n' "$(printable "${BIOS:-unknown}")"
    printf '# boot id         : %s\n' "${BOOT_ID:-unknown}"
    printf '# suspends (boot) : %s\n' "${SUSPENDS:-unknown}"
    printf '# codec           : %s card %s address %s, %s rev %s, vendor %s, subsystem %s\n' \
        "${DEV#"$SYSROOT"}" "$CARD" "$ADDR" "${CHIP:-?}" "${REVISION:-?}" "$VENDOR_ID" "$SUBSYSTEM_ID"
    printf '# fix loaded      : %s (init_verbs %s)%s\n' "$FIX_LOADED" \
        "$([[ $FIX_LOADED == yes ]] && echo present || echo empty)" \
        "$( ((FORCE == 1)) && echo ', consistency checks skipped with --force')"
    printf '# rendered fix    : %s\n' "$FIX_MATCH"
    printf '# snd_hda_intel   : patch=%s model=%s power_save=%s\n' "$P_PATCH" "$P_MODEL" "$P_POWER_SAVE"
    printf '# hwdep sysfs (%s):\n' "${CODEC_SYSFS#"$SYSROOT"}"
    printf '#   modelname          : %s\n' "$(one_line "$MODELNAME")"
    printf '#   init_verbs         : %s\n' "$(one_line "$INIT_VERBS")"
    printf '#   init_pin_configs   : %s\n' "$(one_line "$INIT_PINS")"
    printf '#   driver_pin_configs : %s\n' "$(one_line "$DRIVER_PINS")"
    printf '#   user_pin_configs   : %s\n' "$(one_line "$USER_PINS")"
    printf '#   hints              : %s\n' "$(one_line "$HINTS")"
    printf '# kernel log, this boot:\n'
    while IFS= read -r x; do printf '#   %s\n' "$x"; done <<<"$KLOG"
    printf '#\n# COEFs on NID %s:\n' "$COEF_NID"
    for idx in "${INDEXES[@]}"; do
        v=${VALUE[$idx]}
        note=
        if [[ $idx == 0x1b ]]; then
            if ((v & COEF_1B_FIX_BIT)); then note="   bit 10 (0x0400) SET"; else note="   bit 10 (0x0400) CLEAR"; fi
        fi
        printf '#   %s = %s%s\n' "$idx" "$(hex4 "$v")" "$note"
    done
    if [[ $PHASE == A && -n ${VALUE[0x1b]:-} ]]; then
        printf '#\n# With the current [audio.fix] rule (set 0x0400 on 0x1b) the renderer would\n'
        printf '# write COEF 0x1b = %s (cold value | 0x0400) as the [verb] lines\n' \
            "$(hex4 $((VALUE[0x1b] | COEF_1B_FIX_BIT)))"
        printf '#   %s 0x500 0x001b\n#   %s 0x400 %s\n' "$COEF_NID" "$COEF_NID" "$(hex4 $((VALUE[0x1b] | COEF_1B_FIX_BIT)))"
        printf '# The renderer (apexctl profile render) is authoritative.\n'
    fi
    if [[ $PHASE != A ]]; then
        printf '#\n'
        if [[ -z $REF_FILE ]]; then
            printf '# No phase-A report in %s to compare with.\n' "$OUTDIR"
        else
            printf '# Compared with phase A (%s):\n' "${REF_FILE##*/}"
            for idx in "${INDEXES[@]}"; do
                ref=${REF[$idx]:-}
                if [[ -z $ref ]]; then
                    printf '#   %s: not in the phase-A report\n' "$idx"
                elif ((ref == VALUE[$idx])); then
                    printf '#   %s: same as A (%s)\n' "$idx" "$(hex4 "$ref")"
                else
                    x=$((ref ^ VALUE[$idx]))
                    printf '#   %s: DIFFERS, A %s -> %s %s, changed bits %s (bit %s)\n' "$idx" \
                        "$(hex4 "$ref")" "$PHASE" "$(hex4 "${VALUE[$idx]}")" "$(hex4 "$x")" "$(bits_of "$x")"
                fi
            done
        fi
    fi
    printf '#\n# TOML fragment for hardware/m7400qc/profile.toml:\n'
    case $PHASE in
    A)
        if [[ -z $BIOS ]]; then
            printf '# NOTE: the BIOS version could not be read; replace "unknown" below.\n'
        elif ! provenance_ok "$BIOS"; then
            printf '# NOTE: apexctl accepts 1 to %d printable ASCII characters for bios; edit it.\n' "$MAX_PROVENANCE_LEN"
        fi
        if ! provenance_ok "$KERNEL"; then
            printf '# NOTE: apexctl accepts 1 to %d printable ASCII characters for kernel; edit it.\n' "$MAX_PROVENANCE_LEN"
        fi
        printf '[audio.measurements]\n'
        printf 'date = %s\n' "$TODAY"
        printf 'bios = %s\n' "$(toml_string "${BIOS:-unknown}")"
        printf 'kernel = %s\n' "$(toml_string "$KERNEL")"
        ;;
    B)
        printf '# Add under [audio.measurements], next to cold_boot.\n'
        ;;
    *)
        printf '# Informative only: apexctl accepts this key under [audio.measurements] and\n'
        printf '# does not render it. Add it there if you want the result recorded.\n'
        ;;
    esac
    local items=()
    for idx in "${INDEXES[@]}"; do
        items+=("{ index = $idx, value = $(hex4 "${VALUE[$idx]}") }")
    done
    local joined
    joined=$(printf '%s, ' "${items[@]}")
    printf '%s = [ %s ]\n' "$KEY" "${joined%, }"
}

# Refuse a symlinked or foreign-owned output directory (/var/tmp is world-writable).
if [[ -L $OUTDIR ]]; then
    die "$EXIT_REFUSED" "$OUTDIR is a symlink; refusing to write there"
fi
if [[ -e $OUTDIR ]]; then
    [[ -d $OUTDIR ]] || die "$EXIT_REFUSED" "$OUTDIR exists and is not a directory"
    read -r owner mode < <(stat -c '%u %a' -- "$OUTDIR")
    ((owner == EUID)) || die "$EXIT_REFUSED" "$OUTDIR is owned by uid $owner, not $EUID; refusing to write there"
    ((8#$mode & 8#022)) && die "$EXIT_REFUSED" "$OUTDIR is group- or world-writable (mode $mode); refusing to write there"
else
    install -d -m 0755 -- "$OUTDIR"
fi

umask 022
OUTFILE=$OUTDIR/$PHASE-$STAMP.txt
PARTIAL=$(mktemp -- "$OUTDIR/.$PHASE-$STAMP.XXXXXX")
report >"$PARTIAL"
chmod 0644 -- "$PARTIAL"
mv -f -- "$PARTIAL" "$OUTFILE"
cat -- "$OUTFILE"
printf '\n# Saved to %s\n' "$OUTFILE"
