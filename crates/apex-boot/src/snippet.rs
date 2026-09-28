//! The GRUB boot counting snippet `08_apex-bootcount.cfg` and an executable model of it,
//! which the integration tests hold to the real snippet text.

use crate::grubenv::{EditError, GrubEnv};
use crate::vars::BootVar;

pub const SNIPPET: &str = include_str!("../data/08_apex-bootcount.cfg");

pub const MARKER: &str = "# apex-bootcount v1";

pub const MARKER_PREFIX: &str = "# apex-bootcount v";

/// Bump it with [`MARKER`] whenever the snippet's behaviour changes. bootupd installs the
/// snippet only at `bootc install`, so a new image must keep working with older snippets.
pub const MARKER_VERSION: u32 = 1;

/// Raising it fails the required check on every machine with an older snippet, so the
/// image is judged red, and rolled back, there.
pub const MIN_COMPATIBLE_VERSION: u32 = 1;

const _: () = assert!(MIN_COMPATIBLE_VERSION <= MARKER_VERSION);

/// Sorts before `10_blscfg.cfg`, so it runs before the boot entries are created.
pub const FILE_NAME: &str = "08_apex-bootcount.cfg";

pub const INSTALL_PATH: &str = "/usr/lib/bootupd/grub2-static/configs.d/08_apex-bootcount.cfg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEntry {
    Default,
    Previous,
}

impl BootEntry {
    pub const fn index(self) -> usize {
        match self {
            BootEntry::Default => 0,
            BootEntry::Previous => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootChoice {
    pub entry: BootEntry,
    /// Booted instead when GRUB cannot load `entry`; never saved to grubenv.
    pub fallback: Option<BootEntry>,
}

/// What the snippet does at boot, with its `save_env` calls applied to `env`.
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
        assert!(FILE_NAME < "10_blscfg.cfg");
    }

    #[test]
    fn snippet_uses_only_apex_variables() {
        let code: Vec<&str> = SNIPPET
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        for line in &code {
            assert!(!line.contains("boot_success"), "{line}");
            assert!(!line.contains("boot_counter"), "{line}");
        }
        for var in BootVar::ALL {
            assert!(code.iter().any(|l| l.contains(var.name())), "{var}");
        }
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
        assert_eq!(grub_boot(&mut env).unwrap(), plain);
        assert_eq!(env.get("apex_counter"), Some(&b"3"[..]));

        env.set("apex_target", TARGET).unwrap();
        env.set("apex_ok", "1").unwrap();
        assert_eq!(grub_boot(&mut env).unwrap(), plain);
        assert_eq!(env.get("apex_counter"), Some(&b"3"[..]));
        assert_eq!(env.get("apex_ok"), Some(&b"0"[..]));
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
