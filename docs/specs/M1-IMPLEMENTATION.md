# M1 implementation contract

This file fixes every name, path and format that more than one component relies on.
Change it first (in the same change) whenever a contract changes. The reasoning lives
in `docs/ARCHITECTURE.md` and the ADRs; this file is only the contract.

## 1. Identifiers

| What | Value |
|---|---|
| Image repositories | `ghcr.io/apex-linux/apex` (stable, release key), `ghcr.io/apex-linux/apex-testing` (CI key) |
| Channel tags | `45` in both repositories; immutable `45.YYYYMMDD.RUN` in `apex`; `sha-<git12>-<run>` in `apex-testing` |
| Version string | `<fedora>.<yyyymmdd>.<run>`, e.g. `45.20260924.117`; compared numerically field by field |
| System bus name / object | `io.github.apex_linux.Apex.Update1` at `/io/github/apex_linux/Apex/Update1` |
| Interface XML | `crates/apex-dbus/data/io.github.apex_linux.Apex.Update1.xml` (authoritative) |
| Polkit action ids (lowercase) | `io.github.apex-linux.apex.update.{check,download,apply,discard,rollback}` |
| OCI label prefix | `io.github.apex-linux.apex.` (`degraded`, `content-key`, `fedora`) plus standard `org.opencontainers.image.{version,revision,source,created}` |
| GSettings schema prefix | `io.github.apex_linux.Apex` (M2) |
| State directory | `/var/lib/apex` (0700 root) |

## 2. Installed files (produced by `make install DESTDIR=… PREFIX=/usr`)

| Source | Installed as |
|---|---|
| `target/release/apexctl` | `/usr/bin/apexctl` |
| `target/release/apex-updated` | `/usr/libexec/apex/apex-updated` |
| `target/release/apex-boot-assess` | `/usr/libexec/apex/apex-boot-assess` |
| `target/release/apex-session-agent` | `/usr/libexec/apex/apex-session-agent` |
| `crates/apex-dbus/data/io.github.apex_linux.Apex.Update1.xml` | `/usr/share/dbus-1/interfaces/` |
| `crates/apex-updated/data/apex-updated.service` | `/usr/lib/systemd/system/` |
| `crates/apex-updated/data/apex-update-auto.service`, `.timer` | `/usr/lib/systemd/system/` |
| `crates/apex-updated/data/io.github.apex_linux.Apex.Update1.service` | `/usr/share/dbus-1/system-services/` |
| `crates/apex-updated/data/io.github.apex_linux.Apex.Update1.conf` | `/usr/share/dbus-1/system.d/` |
| `crates/apex-updated/data/io.github.apex-linux.apex.update.policy` | `/usr/share/polkit-1/actions/` |
| `crates/apex-updated/data/apex.tmpfiles.conf` | `/usr/lib/tmpfiles.d/apex.conf` |
| `crates/apex-boot/data/apex-boot-assess.service` | `/usr/lib/systemd/system/` |
| `crates/apex-boot/data/08_apex-bootcount.cfg` | `/usr/lib/bootupd/grub2-static/configs.d/08_apex-bootcount.cfg` |
| `crates/apex-boot/data/emergency.service.d/50-apex-bootcount.conf` | `/usr/lib/systemd/system/emergency.service.d/` |
| `crates/apex-boot/data/rescue.service.d/50-apex-bootcount.conf` | `/usr/lib/systemd/system/rescue.service.d/` |
| `crates/apex-session-agent/data/apex-session-agent.service` | `/usr/lib/systemd/user/` |
| `crates/apex-session-agent/data/80-apex.preset` | `/usr/lib/systemd/user-preset/` |

Image-owned files (not in the RPM) live under `image/system_files/` and are copied into
the rootfs by the Containerfile. The strict bootc policy is one of them:
`/usr/share/apex/containers/policy.json` plus the public keys in
`/usr/share/apex/keys/{release,release-backup,ci}.pub`.

## 3. `manifest.json` (written into every image at `/usr/share/apex/manifest.json`)

Produced in the final image stage by `apexctl build manifest --root / --version … --revision … --base …`.

```json
{
  "schema": 1,
  "name": "apex",
  "version": "45.20260924.117",
  "fedora": 45,
  "revision": "0123456789abcdef0123456789abcdef01234567",
  "base_image": "registry.fedoraproject.org/fedora-bootc@sha256:…",
  "degraded": ["audio"],
  "held": ["gnome-shell"],
  "packages": [
    { "name": "bash", "epoch": 0, "version": "5.3.0", "release": "2.fc45", "arch": "x86_64" }
  ]
}
```

- `packages` is sorted by `(name, arch)` in byte order and is unique on that key.
- `degraded` is sorted and holds any of `adwaita`, `audio`, `files`, `fingerprint`,
  `nvidia`, `settings`, `shell` and `terminal`.
  `nvidia` (S1c) marks an image the owner published without the NVIDIA set, because the
  kmod could not follow the kernel: the dGPU stays unbound in D3cold (ADR 0015 §4).
  `shell` (S1a) marks an image with Fedora's gnome-shell and mutter: the owner chose
  stock for the gnome-shell family at a hold limit (`just pkg-stock gnome-shell`), so
  the shell has no Apex layer, theme, dock or glass until the rebase (ARCHITECTURE
  §4.3). `terminal` (T1) marks an image with Fedora's Ptyxis: its family had no good
  Apex build, so the terminal has no sessions sidebar or pane headers (ADR 0014,
  Consequences). `files` and `settings` mark Fedora's nautilus and gnome-control-center
  the same way, without the Apex Files and Settings work. `adwaita` marks Fedora's
  libadwaita: host apps take a new window radius at their next start, not live. A rebuild-lane family that is stock or degraded needs its name here
  before an image can ship it; the build fails otherwise.
- `held` (S1a) is sorted and lists the rebuild-lane families (`packaging/pkgs/*/pkg.env`
  `family=`, for example `gnome-shell`) that the lane's lock holds at their last good
  build because the newer Fedora build could not be rebuilt (ARCHITECTURE §4.3). It is
  empty when nothing is held. A manifest written before S1a has no `held` field; it
  reads as empty.
- Consumers must ignore unknown fields. `schema` increases only on breaking change.
- The image build copies `degraded` and `held` into the
  `io.github.apex-linux.apex.degraded` and `io.github.apex-linux.apex.held` labels
  (comma-separated) at rechunk time. `held` and `shell` come from the rebuild lane's
  lock, which the image also carries as `/usr/share/apex/rpms.lock` (packaging/README.md,
  "The image").

## 4. Persisted daemon state (`/var/lib/apex`, JSON, atomic write = temp file + fsync + rename)

| File | Content |
|---|---|
| `intent.json` | `{ "digest": "sha256:…", "created": <unix seconds> }`, present only between `Apply` and the boot assessment of that digest |
| `denylist.json` | `{ "digests": [ { "digest": "sha256:…", "reason": "…", "at": <unix seconds> } ] }` |
| `last-check.json` | `{ "at": <unix>, "remote_digest": "sha256:…" or null, "remote_version": "…" or null, "remote_size": <bytes> or null }`; `remote_size` is the new-layer size `bootc upgrade --check` printed, rounded, and becomes `download-size` |
| `health-failure.json` | `{ "digest": "sha256:…", "failed_checks": ["…"], "at": <unix> }`; written by `apex-boot-assess` on a red armed boot, or by `apex-updated` on a `red` report when absent; removed by `AcknowledgeHealthFailure` and by a green boot |
| `settings.json` | `{ "automatic_downloads": true or false }`, written by `SetAutomaticDownloads`; a missing file or key means true |
| `last-fallback.json` | `{ "digest": "sha256:…", "at": <unix> }`, written when a `fallback` report denylists a digest (`LastFallback`); removed by `AcknowledgeHealthFailure` |
| `force-red` | qualification hook only: a root-owned regular file (mode 0600, at most 4 KiB) holding one digest; when it names the booted digest, `apex-boot-assess` adds the failing required check `forced-red` |

Everything else is derived from `bootc status --format=json --format-version=1`.

## 5. GRUB environment variables (owned by `apex-boot`)

Apex uses its own variable names. Fedora's `grub2-set-bootflag` and
`grub-boot-success.timer` write `boot_success`, and greenboot uses `boot_counter`;
sharing those names would let a login mark a failed armed boot as good.

| Variable | Set by | Meaning |
|---|---|---|
| `apex_target` | set by `apex-updated` on `Apply`; unset by `apex-boot-assess` (green, inconsistent, fallback after the report) and by `apex-updated` (after its own fallback rollback) | digest of the deployment that must prove itself |
| `apex_counter` | 3 from `apex-updated` on `Apply`; decremented by the GRUB snippet; 3 again as the fallback hedge written by `apex-boot-assess`; unset together with `apex_target` | remaining attempts; `-1` after GRUB fell back |
| `apex_ok` | 1 from `apex-boot-assess` on green; 0 from `apex-boot-assess` before a red reboot; reset to 0 by the GRUB snippet at every boot, after the counting | userspace confirmation |

While armed, the snippet also sets GRUB's `fallback=1`, so a kernel that GRUB cannot
load falls through to the previous deployment immediately.

Arming precondition (the daemon's arming precondition plus `apex_boot::check_arming`):
- the booted deployment comes from an image;
- a different image is staged;
- grubenv is not armed for the booted digest;
- `health-failure.json` does not name the booted digest.

After finalization ostree makes the staged deployment entry 0 and keeps only the booted
one as entry 1. Entry 1 therefore always exists, and it must be a confirmed deployment.

`Apply` never installs the running build or an older one, and it always arms.

Writes use `grub2-editenv /boot/grub2/grubenv set|unset …`. bootc mounts `/boot`
read-only, so every write runs in a private mount namespace:
`unshare --mount --propagation private` → `mount -o remount,rw /boot` →
`grub2-editenv`. The host never sees a writable `/boot`. Reads parse the environment
block directly (`# GRUB Environment Block` header, `key=value` lines, `#` padding).

## 6. bootc invocation rules (`apex-bootc`)

- Executable `/usr/bin/bootc`, overridable only in debug builds via `APEX_BOOTC`.
- `env_clear()`, then `PATH=/usr/bin:/usr/sbin`, `LANG=C.UTF-8`,
  `CONTAINERS_POLICY_JSON=/usr/share/apex/containers/policy.json`.
- argv is built only from enum variants and compile-time constants:
  `status --format=json --format-version=1`, `upgrade --check`,
  `upgrade --download-only [--progress-fd 3]`, `upgrade --from-downloaded`,
  `rollback`, `switch --enforce-container-sigpolicy --download-only <compile-time ref>`.
- Timeouts: status 30 s, check 5 min, download 2 h, others 10 min. Cancellation
  kills the process group.

## 7. `apexctl` command surface (M1)

```
apexctl status [--json]                    daemon state, booted/staged/rollback, degraded, held; JSON schema 2
apexctl update check
apexctl update download [--force]         --force overrides power/metered gates only
apexctl update apply [--digest D]         default: the staged digest shown by status; prints "reboot to finish"
apexctl update rollback [--mark-bad]
apexctl update changes                    package diff booted → staged
apexctl update acknowledge                clear HealthFailure after reading it
apexctl update automatic                  (root, used by the timer)
apexctl hw report [--json]                read-only: audio init_verbs, charge threshold, dGPU runtime PM, fingerprint device
apexctl hw audio snapshot [--proc FILE]   parse /proc/asound/card*/codec#* (pins, COEFs if dumped, init_verbs)
apexctl profile render [--profile P] [--out DIR] [--check]
apexctl build manifest --root DIR --version V --revision R --base B [--degraded X]... [--held FAMILY]...
```

`profile render` writes into `hardware/<machine>/generated/`:
`monitors.xml` always; `alc294-<machine>.fw` and `apex-audio.conf` only when audio
measurements exist. `--check` exits 1 and prints a diff when committed files differ.

## 8. Polkit table

| Action | allow_any | allow_inactive | allow_active |
|---|---|---|---|
| check, download, apply, discard (AcknowledgeHealthFailure) | no | no | yes |
| rollback | no | no | auth_admin |
| configure (SetAutomaticDownloads) | no | no | auth_admin |

`Automatic` and `ReportBoot` are accepted only from uid 0 (checked with the bus
credentials, not polkit). uid 0 is allowed every action without polkit. No action
installs a downgrade: `Apply` refuses one, and `Rollback` is the way back.

## 9. Crate ownership and boundaries

| Crate | Depends on (internal) | May use tokio | May use GTK |
|---|---|---|---|
| `apex-bootc` | — | yes (process runner) | no |
| `apex-update` | — (pure; its own input types) | no | no |
| `apex-boot` | `apex-bootc`, `apex-dbus` (used by the binary; the package depends on them, the library code does not use them) | binary only | no |
| `apex-dbus` | — | no (zbus proxy is runtime-agnostic) | no |
| `apex-updated` | all of the above | yes (current_thread) | no |
| `apexctl` | `apex-dbus`, `apex-update` (for `changes` formatting), `apex-bootc` (types only) | yes (current_thread, for zbus) | no |

Rendered hardware files live in `hardware/<machine>/generated/`. The legacy allowlist is
`image/audit/legacy-allowlist.txt` (`name<TAB>milestone<TAB>reason`).

## 10. `apex-session-agent` (user service)

| What | Contract |
|---|---|
| Apex keys read (`io.github.apex_linux.Apex.Desktop`) | `window-corner-radius`, `layer-enabled`, `lower-refresh-rate-on-battery`; each feature is off, with a warning, when its key is missing |
| Files written | `$XDG_CONFIG_HOME/gtk-4.0/apex.css` and its `gtk.css` import, `$XDG_DATA_HOME/org.gnome.Ptyxis/palettes/apex.palette`, `$XDG_STATE_HOME/apex/updates-notified` and `$XDG_STATE_HOME/apex/failure-notified` (one digest and a newline each); atomic, and only inside those three homes |
| Crash-guard offer, "Updates Ready" and the update-failure notice | `org.freedesktop.Notifications` on the session bus. The agent follows `Update1.Staged`, `HealthFailure` and `LastFallback` on the system bus (`GetAll` when the name appears, then `PropertiesChanged`) and never starts `apex-updated`. Updates Ready: once per staged, unapplied digest; its action calls `org.gnome.Settings` `launch-panel ('system', [])`. The failure notice: once per digest when `LastFallback` or `HealthFailure` becomes non-empty (the fallback wins), withdrawn when both are cleared; its action calls `launch-panel ('system', ['software-updates'])` |
| Refresh rate on battery | while the key is true and UPower's `OnBattery` is true, the built-in panel (mutter's `is-builtin`) gets its lowest mode of 60 Hz or more at the current resolution; on AC, or with the key false, the highest one, but only after the agent lowered it. `ApplyMonitorsConfig` with method 1 (temporary, monitors.xml untouched), every logical monitor's position, scale, transform and primary flag kept. Nothing happens without a built-in panel with two rates, without UPower, or while a mode the user picked stands (until the next power or key change, or a hotplug) |
