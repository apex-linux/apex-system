# Apex

Apex is a personal, image-based Linux distribution for exactly one laptop: the ASUS
Vivobook Pro 14X OLED **M7400QC** (Ryzen 5 5600H, RTX 3050 Mobile, Realtek ALC294,
ELAN fingerprint reader, Samsung 2880×1800 OLED at 90 Hz).

- **Base:** Fedora 45 delivered with bootc. Updates are atomic and signed, the laptop
  never reboots behind your back, and a bad update rolls back after a counted number of
  failed boots.
- **Desktop:** GNOME 51 on Wayland only, with GTK4/libadwaita apps. Legacy components
  are removed and kept out by a CI gate.
- **Display:** pixel-exact by default (integer scale 2 on the built-in panel).
- **Hardware:** fixes for this machine: ALC294 speakers, the ELAN `04f3:0c6e` reader,
  firmware, dGPU power gating and the battery charge limit. Each fix is meant to go
  upstream.
- **Components:** everything Apex writes itself is Rust.

> Status: **M0, foundation.** Nothing here is ready to install yet. See
> [docs/ROADMAP.md](docs/ROADMAP.md).

## Read first

- [Architecture](docs/ARCHITECTURE.md): how the pieces fit, and why.
- [Decisions](docs/adr/): one ADR per non-obvious choice.
- [M1 contract](docs/specs/M1-IMPLEMENTATION.md): names, paths and formats shared
  between components.
- [Threat model](docs/THREAT-MODEL.md).
- Hardware procedures and acceptance tests: [docs/hardware/](docs/hardware/).
- [Install](docs/INSTALL.md) and [recovery](docs/RECOVERY.md) runbooks.

## Layout

```
crates/     Rust: apex-bootc, apex-update, apex-boot, apex-dbus, apex-updated, apexctl
hardware/   machine profile (single source of truth) and rendered drop-ins
image/      Containerfile, package lists, rootfs overlay, image checks
packaging/  RPM spec, libfprint patch series, kernel overlay (M2)
.github/    CI: rust, rpms, libfprint, image, qualify, canaries
```

## Development

The development host needs only a Rust toolchain. `rust-toolchain.toml` pins the same
version Fedora 45 ships.

```bash
just check
```

```bash
just render
```

Image builds run in CI, or in a Fedora 45 builder VM (see ADR 0013). Hardware
measurements need root and are run by hand. Nothing in this repository runs `sudo` for
you.

## License

GPL-3.0-or-later. Patches in `packaging/` carry the licenses of the projects they
modify (GPL-2.0 for the kernel, LGPL-2.1-or-later for libfprint).
