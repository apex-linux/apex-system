# ALC294 audio tools

Tools for the measured speaker fix ([ADR 0004](https://github.com/apex-linux/apex/blob/main/docs/adr/0004-audio-hda-patch-firmware.md)).
The full procedure, the findings and the upstream path are in
[docs/hardware/AUDIO.md](https://github.com/apex-linux/apex/blob/main/docs/hardware/AUDIO.md).

| File | What it does |
|---|---|
| `coef-dump.sh PHASE` | Reads COEFs 0x0f, 0x1b and 0x40 of the ALC294 (read-only), prints a summary and a TOML fragment for `../profile.toml` in the keys `apexctl` accepts, and saves both to `/var/tmp/apex-audio/`. For C–E it checks that the loaded `init_verbs` are the `[verb]` lines of `../generated/alc294-m7400qc.fw`. Phases A–E are explained in `--help`. |
| `try-fix.sh install\|revert\|status` | Validates the rendered `../generated/alc294-m7400qc.fw` (only what `apexctl profile render` writes: zero-padded `[verb]` lines in the kernel's `init_verbs` format) and `apex-audio.conf`, and copies them to `/lib/firmware/apex/` and `/etc/modprobe.d/` on the current host, or removes exactly those two files. `status` compares the loaded `init_verbs` with the rendered file. |

The rendered files come from `just render` (`apexctl profile render`) and exist only
once `profile.toml` holds the measurements. Both scripts need root and run on the
Ubuntu install of this laptop today (kernel with `CONFIG_SND_HDA_PATCH_LOADER=y`,
`hda-verb` from alsa-tools).

Quick path:

```sh
sudo ./coef-dump.sh A        # cold boot, no fix
sudo ./coef-dump.sh B        # the failing state of AUDIO.md §2, no fix
# paste the fragments into ../profile.toml, then: just render
sudo ./try-fix.sh install    # then phases C, D and E, see AUDIO.md §5
sudo ./try-fix.sh revert     # when done testing
```

Both scripts have test hooks, environment variables never needed on the laptop, that
point them at a fake sysfs tree (and, through `PATH`, a fake `hda-verb`), so they can be
tested without touching the sound hardware:

| Variable | Script | Effect |
|---|---|---|
| `APEX_AUDIO_SYSROOT` | `coef-dump.sh` | prefixes `/sys`, `/proc` and `/dev` |
| `APEX_AUDIO_ALLOW_NONROOT=1` | `coef-dump.sh` | skips the root check; only together with `APEX_AUDIO_SYSROOT` |
| `APEX_AUDIO_OUTDIR` | `coef-dump.sh` | replaces `/var/tmp/apex-audio` |
| `APEX_AUDIO_GENERATED` | `coef-dump.sh` | replaces `../generated` |
| `APEX_TRYFIX_ROOT` | `try-fix.sh` | prefixes every host path |
| `APEX_TRYFIX_ALLOW_NONROOT=1` | `try-fix.sh` | skips the root check; only together with `APEX_TRYFIX_ROOT` |
| `APEX_TRYFIX_GENERATED` | `try-fix.sh` | replaces `../generated` |

Exit status of both: 0 ok, 1 usage, 2 refused (a precondition or a file check failed);
`coef-dump.sh` also exits 3 when `hda-verb` fails or gives no stable, parsable read, and
70 on an internal error.
