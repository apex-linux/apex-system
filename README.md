# apex-system

The update and boot stack of [Apex](https://github.com/apex-linux/apex), a Fedora 45
bootc desktop for one laptop, the ASUS Vivobook Pro 14X OLED (M7400QC). Everything here
is Rust and ships in the `apex` RPM.

| Crate | What it is |
|---|---|
| `apex-updated` | The update service on the system bus (`io.github.apex_linux.Apex.Update1`), the only component that runs bootc. One queue, polkit for every action, and it never reboots |
| `apexctl` | The command-line tool: update control through `apex-updated`, read-only hardware reports, and hardware-profile rendering |
| `apex-boot` | Boot assessment (`apex-boot-assess`): the GRUB environment codec, the boot counter, the health checks and the counted rollback |
| `apex-update` | The pure update engine: gates, state derivation, version ordering and the persisted state |
| `apex-bootc` | bootc as a library: the lenient status model, progress events, and argv built from enums |
| `apex-dbus` | The `Update1` D-Bus contract and its proxy |

`hardware/m7400qc/profile.toml` is the single source of the machine's facts. `apexctl`
compiles them in, and `just render` writes `hardware/m7400qc/generated/`, which the
image installs. [docs/specs/M1-IMPLEMENTATION.md](docs/specs/M1-IMPLEMENTATION.md) is
the contract of names, paths and formats the crates share with the image.

The design behind it is in the [apex](https://github.com/apex-linux/apex) repository:
[ARCHITECTURE §5](https://github.com/apex-linux/apex/blob/main/docs/ARCHITECTURE.md#5-updates),
the [ADRs](https://github.com/apex-linux/apex/tree/main/docs/adr) and the
[threat model](https://github.com/apex-linux/apex/blob/main/docs/THREAT-MODEL.md).
[apex-image](https://github.com/apex-linux/apex-image) pins this repository by commit
and builds the RPM in its builder VM.

## Development

The host needs only a Rust toolchain. `rust-toolchain.toml` pins the release Fedora 45
ships, the one that builds the RPM.

```bash
just check     # fmt, clippy -D warnings, the tests and the comment guard, as CI runs them
just deny      # advisories, licences, bans and sources
just render    # hardware/m7400qc/generated from the profile
just rpm       # the RPM, on Fedora 45 (packaging/README.md)
```

The comment guard is `apex-lint`, from
[apex-ci](https://github.com/apex-linux/apex-ci):
`cargo install --locked --git https://github.com/apex-linux/apex-ci apex-lint`.
Code explains itself, and comments say only why
([CONTRIBUTING](https://github.com/apex-linux/apex/blob/main/docs/CONTRIBUTING.md)).

## License

GPL-3.0-or-later (`LICENSE`).
