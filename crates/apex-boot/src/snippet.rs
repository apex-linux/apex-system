//! The GRUB boot counting snippet `08_apex-bootcount.cfg` and an executable model of it.
//!
//! The snippet is installed as a bootupd static config fragment and becomes part of
//! `/boot/grub2/grub.cfg` at `bootc install` time. [`grub_boot`] is the same logic in Rust.
//! Simulations use it, and the integration tests run the real snippet text through a small
//! GRUB-script interpreter to prove the two agree.

use crate::grubenv::{EditError, GrubEnv};
use crate::vars::BootVar;

/// The snippet, exactly as installed.
pub const SNIPPET: &str = include_str!("../data/08_apex-bootcount.cfg");

/// The snippet's first line: [`MARKER_PREFIX`] and [`MARKER_VERSION`].
pub const MARKER: &str = "# apex-bootcount v1";

/// Everything in [`MARKER`] before the version number.
pub const MARKER_PREFIX: &str = "# apex-bootcount v";

/// The version of the snippet this image ships. Bump it (and [`MARKER`]) whenever the
/// snippet's behaviour changes.
///
/// bootupd installs the snippet into `/boot/grub2/grub.cfg` only at `bootc install`, so
/// every existing machine keeps running the version it was installed with until
/// `apexctl boot repair-grub` (M2) reinstalls it. A new image must therefore keep working
/// with older snippets: bumping this version alone only makes the reported check
/// [`crate::checks::names::GRUB_CFG_CURRENT`] ask for a reinstall.
pub const MARKER_VERSION: u32 = 1;

/// The oldest snippet version this image's decision table works with. Raise it only when
/// the decision table can no longer drive an older snippet safely: the required check
/// [`crate::checks::names::GRUB_CFG_MARKER`] then fails on every machine that still has an
/// older one, so the image's own first boots are judged red there (and, with nobody
/// logged in, rolled back).
pub const MIN_COMPATIBLE_VERSION: u32 = 1;

// An image always understands the snippet it ships.
const _: () = assert!(MIN_COMPATIBLE_VERSION <= MARKER_VERSION);

/// File name inside bootupd's `configs.d`. It sorts before `10_blscfg.cfg`, so it runs
/// before the boot entries are created.
pub const FILE_NAME: &str = "08_apex-bootcount.cfg";

/// Where the RPM installs the snippet (M1 contract §2).
pub const INSTALL_PATH: &str = "/usr/lib/bootupd/grub2-static/configs.d/08_apex-bootcount.cfg";

/// The menu entry GRUB boots after the snippet ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEntry {
    /// Entry 0: the default deployment.
    Default,
    /// Entry 1: the previous (rollback) deployment.
    Previous,
}

impl BootEntry {
    /// The GRUB menu index.
    pub const fn index(self) -> usize {
        match self {
            BootEntry::Default => 0,
            BootEntry::Previous => 1,
        }
    }
}

/// What GRUB boots after the snippet ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootChoice {
    /// The entry GRUB starts (`default`).
    pub entry: BootEntry,
    /// The entry GRUB starts instead when it cannot load [`BootChoice::entry`] (`fallback`),
    /// if the snippet set one.
    pub fallback: Option<BootEntry>,
}

/// What the snippet does at boot, applied to an in-memory grubenv: updates the counter and
/// `apex_ok` as its `save_env` calls do, and returns what GRUB boots.
///
/// - Armed (`apex_target` and `apex_counter` both non-empty) and not confirmed (`apex_ok`
///   is not `1`): `3 → 2`, `2 → 1`, `1 → 0`, `fallback=1` and boot the default entry; any
///   other counter value: boot entry 1 and store `-1`.
/// - Always: `apex_ok=0`, so userspace has to confirm this boot again.
///
/// `fallback` is a runtime variable: it is never saved to grubenv.
pub fn grub_boot(env: &mut GrubEnv) -> Result<BootChoice, EditError> {
    let is_set = |var: BootVar, env: &GrubEnv| env.get(var.name()).is_some_and(|v| !v.is_empty());
    let mut choice = BootChoice {
        entry: BootEntry::Default,
        fallback: None,
    };
    let confirmed = env.get(BootVar::ApexOk.name()) == Some(&b"1"[..]);
    if is_set(BootVar::ApexTarget, env) && is_set(BootVar::ApexCounter, env) && !confirmed {
        let next = match env.get(BootVar::ApexCounter.name()) {
            Some(b"3") => "2",
            Some(b"2") => "1",
            Some(b"1") => "0",
            _ => {
                choice.entry = BootEntry::Previous;
                "-1"
            }
        };
        if choice.entry == BootEntry::Default {
            choice.fallback = Some(BootEntry::Previous);
        }
        env.set(BootVar::ApexCounter.name(), next)?;
    }
    env.set(BootVar::ApexOk.name(), "0")?;
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Digest;
    use crate::vars::{BootVars, CounterState, arm_ops};

    const TARGET: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn marker_is_the_first_line() {
        assert_eq!(SNIPPET.lines().next(), Some(MARKER));
        assert_eq!(MARKER, format!("{MARKER_PREFIX}{MARKER_VERSION}"));
        assert!(INSTALL_PATH.ends_with(FILE_NAME));
        // bootupd concatenates configs.d in byte order; blscfg must come after us.
        assert!(FILE_NAME < "10_blscfg.cfg");
    }

    #[test]
    fn snippet_uses_only_apex_variables() {
        let code: Vec<&str> = SNIPPET
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        for line in &code {
            // grub2-set-bootflag, grub-boot-success.timer and greenboot own these.
            assert!(!line.contains("boot_success"), "{line}");
            assert!(!line.contains("boot_counter"), "{line}");
        }
        for var in BootVar::ALL {
            assert!(code.iter().any(|l| l.contains(var.name())), "{var}");
        }
        // Saved: the counter and the confirmation. Never saved: default and fallback.
        let saved: Vec<&str> = code
            .iter()
            .filter_map(|l| l.trim().strip_prefix("save_env "))
            .collect();
        assert_eq!(saved, ["apex_counter", "apex_ok"]);
    }

    #[test]
    fn snippet_avoids_the_fedora_increment_module() {
        for line in SNIPPET.lines().filter(|l| !l.trim_start().starts_with('#')) {
            assert!(!line.contains("increment"), "{line}");
            assert!(!line.contains("decrement"), "{line}");
        }
    }

    #[test]
    fn a_target_gets_three_boots_then_falls_back() {
        let mut env = GrubEnv::new();
        for op in arm_ops(&Digest::parse(TARGET).unwrap()) {
            op.apply_to(&mut env).unwrap();
        }
        let mut seen = Vec::new();
        for _ in 0..5 {
            let choice = grub_boot(&mut env).unwrap();
            let vars = BootVars::from_env(&env).unwrap();
            assert_eq!(vars.apex_ok, Some(false));
            seen.push((choice.entry, choice.fallback, vars.counter_state()));
        }
        let counting = Some(BootEntry::Previous);
        assert_eq!(
            seen,
            vec![
                (
                    BootEntry::Default,
                    counting,
                    CounterState::Counted { remaining: 2 }
                ),
                (
                    BootEntry::Default,
                    counting,
                    CounterState::Counted { remaining: 1 }
                ),
                (
                    BootEntry::Default,
                    counting,
                    CounterState::Counted { remaining: 0 }
                ),
                (BootEntry::Previous, None, CounterState::Exhausted),
                (BootEntry::Previous, None, CounterState::Exhausted),
            ]
        );
        // fallback only lives in GRUB's memory.
        assert_eq!(env.get("fallback"), None);
    }

    #[test]
    fn confirmed_or_unarmed_boots_are_not_counted() {
        let plain = BootChoice {
            entry: BootEntry::Default,
            fallback: None,
        };
        let mut env = GrubEnv::new();
        env.set("apex_counter", "3").unwrap();
        // No apex_target: not armed (a stray counter is ignored).
        assert_eq!(grub_boot(&mut env).unwrap(), plain);
        assert_eq!(env.get("apex_counter"), Some(&b"3"[..]));

        env.set("apex_target", TARGET).unwrap();
        env.set("apex_ok", "1").unwrap();
        assert_eq!(grub_boot(&mut env).unwrap(), plain);
        assert_eq!(env.get("apex_counter"), Some(&b"3"[..]));
        assert_eq!(env.get("apex_ok"), Some(&b"0"[..]));
        // The confirmation is consumed: the next boot counts.
        assert_eq!(
            grub_boot(&mut env).unwrap(),
            BootChoice {
                entry: BootEntry::Default,
                fallback: Some(BootEntry::Previous),
            }
        );
        assert_eq!(env.get("apex_counter"), Some(&b"2"[..]));
    }

    #[test]
    fn unknown_counter_values_fall_back() {
        for value in ["0", "-1", "4", "03", "x"] {
            let mut env = GrubEnv::new();
            env.set("apex_target", TARGET).unwrap();
            env.set("apex_counter", value).unwrap();
            assert_eq!(
                grub_boot(&mut env).unwrap(),
                BootChoice {
                    entry: BootEntry::Previous,
                    fallback: None,
                },
                "{value}"
            );
            assert_eq!(env.get("apex_counter"), Some(&b"-1"[..]));
        }
    }
}
