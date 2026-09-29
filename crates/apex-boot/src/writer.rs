use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::grubenv::{EditError, GrubEnv};
use crate::process;
use crate::vars::{BootVar, EnvOp, ValueError};

pub const GRUBENV_PATH: &str = "/boot/grub2/grubenv";

pub const GRUB2_EDITENV: &str = "/usr/bin/grub2-editenv";

/// Not `unshare(CLONE_NEWNS)` in-process: it would move only the calling pool thread, and doing
/// it between `fork` and `exec` needs unsafe code.
pub const UNSHARE: &str = "/usr/bin/unshare";

pub const SH: &str = "/usr/bin/sh";

pub const MOUNT: &str = "/usr/bin/mount";

pub const BOOT_MOUNT: &str = "/boot";

/// Constant, so the grubenv path and the arguments reach it only as positional parameters.
/// `-n` skips `/run/mount/utab`, which is read-only under `ProtectSystem=strict`.
pub const REMOUNT_SCRIPT: &str = concat!(
    "/usr/bin/mount -n -o remount,rw /boot",
    " && exec /usr/bin/grub2-editenv \"$0\" \"$@\""
);

pub const EDITENV_TIMEOUT: Duration = Duration::from_secs(30);

/// Each call is one `grub2-editenv` invocation, which writes the block back once: a single
/// call is the unit of consistency.
pub trait GrubEnvWriter {
    fn set(&mut self, assignments: &[(&str, &str)]) -> Result<(), WriteError>;

    fn unset(&mut self, names: &[&str]) -> Result<(), WriteError>;
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error("running grub2-editenv failed: {message}")]
    Run { message: String },
    #[error("writing grubenv failed ({status}): {stderr}")]
    Failed { status: String, stderr: String },
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error("injected failure")]
    Injected,
}

/// One call per run of consecutive `Set`s or `Unset`s. Stops at the first failed call; the
/// calls before it have taken effect.
pub fn apply_ops<W: GrubEnvWriter + ?Sized>(
    writer: &mut W,
    ops: &[EnvOp],
) -> Result<(), WriteError> {
    let mut rest = ops;
    while let Some(first) = rest.first() {
        let is_set = matches!(first, EnvOp::Set(..));
        let run_len = rest
            .iter()
            .take_while(|op| matches!(op, EnvOp::Set(..)) == is_set)
            .count();
        let (run, tail) = rest.split_at(run_len);
        if is_set {
            let assignments: Vec<(&str, &str)> = run
                .iter()
                .filter_map(|op| match op {
                    EnvOp::Set(var, value) => Some((var.name(), value.as_str())),
                    EnvOp::Unset(_) => None,
                })
                .collect();
            writer.set(&assignments)?;
        } else {
            let names: Vec<&str> = run.iter().map(|op| op.var().name()).collect();
            writer.unset(&names)?;
        }
        rest = tail;
    }
    Ok(())
}

pub fn set_args(assignments: &[(&str, &str)]) -> Result<Vec<OsString>, ValueError> {
    assignments
        .iter()
        .map(|(name, value)| {
            BootVar::from_name(name)?.check_value(value)?;
            Ok(OsString::from(format!("{name}={value}")))
        })
        .collect()
}

pub fn unset_args(names: &[&str]) -> Result<Vec<OsString>, ValueError> {
    names
        .iter()
        .map(|name| BootVar::from_name(name).map(|var| OsString::from(var.name())))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Launch {
    Direct { program: PathBuf, grubenv: PathBuf },
    PrivateBootRw,
}

/// Refuses every name and value but the Apex variables' before anything runs.
#[derive(Debug, Clone)]
pub struct Editenv {
    launch: Launch,
    timeout: Duration,
}

impl Editenv {
    /// Needs `CAP_SYS_ADMIN`.
    pub fn system() -> Self {
        Self {
            launch: Launch::PrivateBootRw,
            timeout: EDITENV_TIMEOUT,
        }
    }

    pub fn for_file(grubenv: impl Into<PathBuf>) -> Self {
        let grubenv = grubenv.into();
        if grubenv == Path::new(GRUBENV_PATH) {
            Self::system()
        } else {
            Self::new(GRUB2_EDITENV, grubenv)
        }
    }

    pub fn new(program: impl Into<PathBuf>, grubenv: impl Into<PathBuf>) -> Self {
        Self {
            launch: Launch::Direct {
                program: program.into(),
                grubenv: grubenv.into(),
            },
            timeout: EDITENV_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn remounts_boot(&self) -> bool {
        self.launch == Launch::PrivateBootRw
    }

    pub fn command_line(&self, verb: &str, args: &[OsString]) -> Vec<OsString> {
        let mut line: Vec<OsString> = match &self.launch {
            Launch::Direct { program, grubenv } => {
                vec![
                    program.clone().into_os_string(),
                    non_option_file_arg(grubenv),
                ]
            }
            Launch::PrivateBootRw => [
                UNSHARE,
                "--mount",
                "--propagation",
                "private",
                "--",
                SH,
                "-c",
                REMOUNT_SCRIPT,
                GRUBENV_PATH,
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        };
        line.push(verb.into());
        line.extend(args.iter().cloned());
        line
    }

    fn run(&self, verb: &str, args: &[OsString]) -> Result<(), WriteError> {
        let line = self.command_line(verb, args);
        let (program, rest) = line.split_first().ok_or_else(|| WriteError::Run {
            message: "empty command line".to_owned(),
        })?;
        let mut command = Command::new(program);
        command
            .args(rest)
            .env_clear()
            .env("PATH", "/usr/bin:/usr/sbin")
            .env("LANG", "C.UTF-8");
        let done = process::run(&mut command, self.timeout).map_err(|err| WriteError::Run {
            message: err.describe(),
        })?;
        if done.status.success() {
            Ok(())
        } else {
            Err(WriteError::Failed {
                status: done.status.to_string(),
                stderr: String::from_utf8_lossy(&done.stderr).trim().to_owned(),
            })
        }
    }
}

fn non_option_file_arg(path: &Path) -> OsString {
    if path.is_absolute() {
        path.as_os_str().to_owned()
    } else {
        Path::new(".").join(path).into_os_string()
    }
}

impl GrubEnvWriter for Editenv {
    fn set(&mut self, assignments: &[(&str, &str)]) -> Result<(), WriteError> {
        let args = set_args(assignments)?;
        if args.is_empty() {
            return Ok(());
        }
        self.run("set", &args)
    }

    fn unset(&mut self, names: &[&str]) -> Result<(), WriteError> {
        let args = unset_args(names)?;
        if args.is_empty() {
            return Ok(());
        }
        self.run("unset", &args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditenvCall {
    Set(Vec<(String, String)>),
    Unset(Vec<String>),
}

/// Validates like [`Editenv`], so a refused call is not recorded: the production writer
/// would not have run anything either.
#[derive(Debug, Clone, Default)]
pub struct RecordingWriter {
    env: GrubEnv,
    calls: Vec<EditenvCall>,
    fail_at: Option<usize>,
}

impl RecordingWriter {
    pub fn new(env: GrubEnv) -> Self {
        Self {
            env,
            calls: Vec::new(),
            fail_at: None,
        }
    }

    /// The failed call is still recorded; the block stays unchanged.
    #[must_use]
    pub fn failing_at(mut self, call: usize) -> Self {
        self.fail_at = Some(call);
        self
    }

    pub fn env(&self) -> &GrubEnv {
        &self.env
    }

    pub fn calls(&self) -> &[EditenvCall] {
        &self.calls
    }

    fn should_fail(&self) -> bool {
        self.fail_at == Some(self.calls.len() - 1)
    }
}

impl GrubEnvWriter for RecordingWriter {
    fn set(&mut self, assignments: &[(&str, &str)]) -> Result<(), WriteError> {
        set_args(assignments)?;
        self.calls.push(EditenvCall::Set(
            assignments
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
        ));
        if self.should_fail() {
            return Err(WriteError::Injected);
        }
        let mut next = self.env.clone();
        for (name, value) in assignments {
            next.set(name, value)?;
        }
        self.env = next;
        Ok(())
    }

    fn unset(&mut self, names: &[&str]) -> Result<(), WriteError> {
        unset_args(names)?;
        self.calls.push(EditenvCall::Unset(
            names.iter().map(|n| (*n).to_owned()).collect(),
        ));
        if self.should_fail() {
            return Err(WriteError::Injected);
        }
        let mut next = self.env.clone();
        for name in names {
            next.unset(name)?;
        }
        self.env = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use crate::digest::Digest;
    use crate::vars::{BootVar, BootVars, arm_ops, disarm_ops};

    const TARGET: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn lossy(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| OsStr::to_string_lossy(arg).into_owned())
            .collect()
    }

    #[test]
    fn arming_is_one_set_call() {
        let mut writer = RecordingWriter::default();
        apply_ops(&mut writer, &arm_ops(&Digest::parse(TARGET).unwrap())).unwrap();
        assert_eq!(
            writer.calls(),
            [EditenvCall::Set(vec![
                ("apex_target".into(), TARGET.into()),
                ("apex_counter".into(), "3".into()),
                ("apex_ok".into(), "0".into()),
            ])]
        );
        let vars = BootVars::from_env(writer.env()).unwrap();
        assert_eq!(vars.apex_counter, Some(3));
    }

    #[test]
    fn runs_of_the_same_kind_are_batched_in_order() {
        let ops = [
            EnvOp::Set(BootVar::ApexOk, "1".into()),
            EnvOp::Unset(BootVar::ApexTarget),
            EnvOp::Unset(BootVar::ApexCounter),
            EnvOp::Set(BootVar::ApexCounter, "2".into()),
        ];
        let mut writer = RecordingWriter::default();
        apply_ops(&mut writer, &ops).unwrap();
        assert_eq!(
            writer.calls(),
            [
                EditenvCall::Set(vec![("apex_ok".into(), "1".into())]),
                EditenvCall::Unset(vec!["apex_target".into(), "apex_counter".into()]),
                EditenvCall::Set(vec![("apex_counter".into(), "2".into())]),
            ]
        );
        apply_ops(&mut writer, &[]).unwrap();
        assert_eq!(writer.calls().len(), 3);
    }

    #[test]
    fn failure_stops_and_leaves_block_unchanged() {
        let mut writer = RecordingWriter::default().failing_at(1);
        let target = Digest::parse(TARGET).unwrap();
        apply_ops(&mut writer, &arm_ops(&target)).unwrap();
        let armed = writer.env().clone();
        let mut ops = disarm_ops();
        ops.push(EnvOp::Set(BootVar::ApexOk, "1".into()));
        assert!(matches!(
            apply_ops(&mut writer, &ops),
            Err(WriteError::Injected)
        ));
        assert_eq!(writer.env(), &armed);
        assert_eq!(writer.calls().len(), 2);
    }

    #[test]
    fn fake_reports_model_errors_atomically() {
        let mut writer = RecordingWriter::new(GrubEnv::empty(40).unwrap());
        let err = writer
            .set(&[("apex_ok", "1"), ("apex_counter", "3")])
            .unwrap_err();
        assert!(matches!(err, WriteError::Edit(EditError::NoSpace { .. })));
        assert_eq!(writer.env().get("apex_ok"), None);
    }

    #[test]
    fn fake_validates_like_the_real_writer() {
        let mut writer = RecordingWriter::default();
        assert!(matches!(
            writer.set(&[("boot_success", "1")]),
            Err(WriteError::Value(ValueError::UnknownVariable { .. }))
        ));
        assert!(matches!(
            writer.set(&[("apex_target", "latest")]),
            Err(WriteError::Value(ValueError::InvalidValue { .. }))
        ));
        assert!(matches!(
            writer.unset(&["boot_counter"]),
            Err(WriteError::Value(ValueError::UnknownVariable { .. }))
        ));
        assert!(writer.calls().is_empty());
    }

    #[test]
    fn editenv_validates_before_running() {
        let mut writer = Editenv::new("/nonexistent/grub2-editenv", "/nonexistent/grubenv");
        for (name, value) in [
            ("bad name", "1"),
            ("boot_success", "1"),
            ("boot_counter", "3"),
            ("-v", "1"),
        ] {
            assert!(
                matches!(
                    writer.set(&[(name, value)]),
                    Err(WriteError::Value(ValueError::UnknownVariable { .. }))
                ),
                "{name}"
            );
        }
        for (name, value) in [
            ("apex_target", "x\0y"),
            ("apex_target", "sha256:abc; reboot"),
            ("apex_counter", "3\n"),
            ("apex_counter", "$(id)"),
            ("apex_ok", "'1'"),
        ] {
            assert!(
                matches!(
                    writer.set(&[(name, value)]),
                    Err(WriteError::Value(ValueError::InvalidValue { .. }))
                ),
                "{name}={value:?}"
            );
        }
        assert!(matches!(
            writer.set(&[("apex_ok", "1"), ("apex_counter", "x")]),
            Err(WriteError::Value(_))
        ));
        assert!(matches!(
            writer.unset(&["-v"]),
            Err(WriteError::Value(ValueError::UnknownVariable { .. }))
        ));
        writer.set(&[]).unwrap();
        writer.unset(&[]).unwrap();
        let err = writer.set(&[("apex_ok", "1")]).unwrap_err();
        assert!(matches!(err, WriteError::Run { .. }), "{err}");
    }

    #[test]
    fn editenv_reports_exit_status() {
        let mut failing =
            Editenv::new("/bin/false", "/dev/null").with_timeout(Duration::from_secs(10));
        let err = failing.unset(&["apex_ok"]).unwrap_err();
        assert!(
            matches!(&err, WriteError::Failed { status, .. } if status.contains('1')),
            "{err}"
        );
    }

    #[test]
    fn system_command_line_is_exact() {
        let writer = Editenv::system();
        assert!(writer.remounts_boot());
        let args = set_args(&[
            ("apex_target", TARGET),
            ("apex_counter", "3"),
            ("apex_ok", "0"),
        ])
        .unwrap();
        assert_eq!(
            lossy(&writer.command_line("set", &args)),
            [
                "/usr/bin/unshare",
                "--mount",
                "--propagation",
                "private",
                "--",
                "/usr/bin/sh",
                "-c",
                "/usr/bin/mount -n -o remount,rw /boot && exec /usr/bin/grub2-editenv \"$0\" \"$@\"",
                "/boot/grub2/grubenv",
                "set",
                &format!("apex_target={TARGET}"),
                "apex_counter=3",
                "apex_ok=0",
            ]
        );
        let args = unset_args(&["apex_target", "apex_counter"]).unwrap();
        assert_eq!(
            lossy(&writer.command_line("unset", &args)),
            [
                "/usr/bin/unshare",
                "--mount",
                "--propagation",
                "private",
                "--",
                "/usr/bin/sh",
                "-c",
                "/usr/bin/mount -n -o remount,rw /boot && exec /usr/bin/grub2-editenv \"$0\" \"$@\"",
                "/boot/grub2/grubenv",
                "unset",
                "apex_target",
                "apex_counter",
            ]
        );
    }

    #[test]
    fn the_script_names_only_constants() {
        assert_eq!(
            REMOUNT_SCRIPT,
            format!("{MOUNT} -n -o remount,rw {BOOT_MOUNT} && exec {GRUB2_EDITENV} \"$0\" \"$@\"")
        );
        assert!(GRUBENV_PATH.starts_with(&format!("{BOOT_MOUNT}/")));
        assert_eq!(REMOUNT_SCRIPT.matches('$').count(), 2);
        assert!(!REMOUNT_SCRIPT.contains(['`', ';', '\'', '\n']));
    }

    #[test]
    fn only_the_contract_file_uses_the_namespace() {
        assert!(Editenv::for_file(GRUBENV_PATH).remounts_boot());
        for other in ["/tmp/grubenv", "/boot/grub2/grubenv.bak", "grubenv"] {
            let writer = Editenv::for_file(other);
            assert!(!writer.remounts_boot(), "{other}");
            let argv = lossy(&writer.command_line("unset", &unset_args(&["apex_ok"]).unwrap()));
            assert_eq!(argv[0], GRUB2_EDITENV);
            assert_eq!(argv[2..], ["unset", "apex_ok"]);
        }
        let writer = Editenv::new("/usr/bin/grub-editenv", "-grubenv");
        assert_eq!(
            lossy(&writer.command_line("unset", &[])),
            ["/usr/bin/grub-editenv", "./-grubenv", "unset"]
        );
    }

    #[test]
    fn the_script_passes_its_arguments_on_and_stops_when_the_remount_fails() {
        let script = |remount: &str| {
            REMOUNT_SCRIPT
                .replace(&format!("{MOUNT} -n -o remount,rw {BOOT_MOUNT}"), remount)
                .replace(GRUB2_EDITENV, "echo")
        };
        let run = |remount: &str| {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script(remount))
                .args(["/boot/grub2/grubenv", "set", "apex_ok=1", "apex_counter=3"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap()
        };
        let ok = run("true");
        assert!(ok.status.success());
        assert_eq!(
            String::from_utf8(ok.stdout).unwrap(),
            "/boot/grub2/grubenv set apex_ok=1 apex_counter=3\n"
        );
        let failed = run("false");
        assert!(!failed.status.success());
        assert!(failed.stdout.is_empty());
    }
}
