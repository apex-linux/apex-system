//! Runs the real `grub2-editenv` (Fedora) or `grub-editenv` (other distributions) when one
//! is installed: the direct form of the production writer ([`Editenv::new`]) for the Apex
//! variables, and the bare tool for arbitrary names and values, and checks that the files
//! it writes are byte for byte what the in-memory model predicts. Without either tool the
//! tests only print that they were skipped. (The namespaced form, [`Editenv::system`],
//! needs root and a read-only `/boot`; its command line is unit-tested in `writer.rs`.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use apex_boot::grubenv::{self, GrubEnv};
use apex_boot::{
    BootVars, Digest, Editenv, GrubEnvWriter, WriteError, apply_ops, arm_ops, disarm_ops,
};
use proptest::prelude::*;

fn editenv_tool() -> Option<PathBuf> {
    ["/usr/bin/grub2-editenv", "/usr/bin/grub-editenv"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn create(tool: &Path, file: &Path) {
    let status = Command::new(tool).arg(file).arg("create").status().unwrap();
    assert!(status.success());
}

#[test]
fn create_matches_the_model() {
    let Some(tool) = editenv_tool() else {
        eprintln!("skipped: no grub-editenv installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("grubenv");
    create(&tool, &file);
    assert_eq!(std::fs::read(&file).unwrap(), GrubEnv::new().to_bytes());
}

#[test]
fn arming_and_confirming_through_the_real_tool() {
    let Some(tool) = editenv_tool() else {
        eprintln!("skipped: no grub-editenv installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("grubenv");
    create(&tool, &file);
    let mut writer = Editenv::new(&tool, &file);
    let target =
        Digest::parse("sha256:5555555555555555555555555555555555555555555555555555555555555555")
            .unwrap();

    apply_ops(&mut writer, &arm_ops(&target)).unwrap();
    let vars = BootVars::from_env(&grubenv::read_file(&file).unwrap()).unwrap();
    assert_eq!(
        vars,
        BootVars {
            apex_target: Some(target.clone()),
            apex_counter: Some(3),
            apex_ok: Some(false),
        }
    );

    let mut model = GrubEnv::new();
    for op in arm_ops(&target) {
        op.apply_to(&mut model).unwrap();
    }
    assert_eq!(std::fs::read(&file).unwrap(), model.to_bytes());

    apply_ops(&mut writer, &disarm_ops()).unwrap();
    let vars = BootVars::from_env(&grubenv::read_file(&file).unwrap()).unwrap();
    assert!(!vars.is_armed());
    assert_eq!(vars.apex_counter, None);

    // `list` shows exactly what the model reads.
    let listed = Command::new(&tool).arg(&file).arg("list").output().unwrap();
    assert_eq!(String::from_utf8(listed.stdout).unwrap(), "apex_ok=0\n");
}

#[test]
fn a_full_block_is_reported_by_the_tool() {
    let Some(tool) = editenv_tool() else {
        eprintln!("skipped: no grub-editenv installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("grubenv");
    create(&tool, &file);
    // Fill the block with a foreign variable, leaving too little room for an arming.
    assert!(tool_set(&tool, &file, "filler", &"x".repeat(850)));
    let before = std::fs::read(&file).unwrap();
    let mut writer = Editenv::new(&tool, &file);
    let target =
        Digest::parse("sha256:5555555555555555555555555555555555555555555555555555555555555555")
            .unwrap();
    let err = apply_ops(&mut writer, &arm_ops(&target)).unwrap_err();
    assert!(matches!(err, WriteError::Failed { .. }), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

/// The production writer refuses what the tool would happily write.
#[test]
fn the_writer_refuses_foreign_variables_before_running_the_tool() {
    let Some(tool) = editenv_tool() else {
        eprintln!("skipped: no grub-editenv installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("grubenv");
    create(&tool, &file);
    let before = std::fs::read(&file).unwrap();
    let mut writer = Editenv::new(&tool, &file);
    for (name, value) in [("boot_success", "1"), ("apex_ok", "1\nboot_success=1")] {
        assert!(matches!(
            writer.set(&[(name, value)]),
            Err(WriteError::Value(_))
        ));
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

/// `<tool> <file> set name=value`, without the writer's validation.
fn tool_set(tool: &Path, file: &Path, name: &str, value: &str) -> bool {
    Command::new(tool)
        .arg(file)
        .arg("set")
        .arg(format!("{name}={value}"))
        .status()
        .unwrap()
        .success()
}

/// `<tool> <file> unset name`, without the writer's validation.
fn tool_unset(tool: &Path, file: &Path, name: &str) -> bool {
    Command::new(tool)
        .arg(file)
        .arg("unset")
        .arg(name)
        .status()
        .unwrap()
        .success()
}

#[derive(Debug, Clone)]
enum Op {
    Set(&'static str, String),
    Unset(&'static str),
}

fn arb_op() -> impl Strategy<Value = Op> {
    let name = prop::sample::select(vec!["a", "ab", "apex_counter", "apex_target"]);
    let value = "[a-z0-9=#\\\\\n\r ]{0,120}";
    prop_oneof![
        3 => (name.clone(), value).prop_map(|(n, v)| Op::Set(n, v)),
        1 => name.prop_map(Op::Unset),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn real_tool_matches_the_model(ops in prop::collection::vec(arb_op(), 1..12)) {
        let Some(tool) = editenv_tool() else { return Ok(()); };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("grubenv");
        create(&tool, &file);
        let mut model = GrubEnv::new();
        for op in &ops {
            match op {
                Op::Set(name, value) => {
                    let tool_ok = tool_set(&tool, &file, name, value);
                    let model_result = model.set(name, value);
                    prop_assert_eq!(tool_ok, model_result.is_ok(), "{:?}", op);
                }
                Op::Unset(name) => {
                    prop_assert!(tool_unset(&tool, &file, name));
                    model.unset(name).unwrap();
                }
            }
            prop_assert_eq!(std::fs::read(&file).unwrap(), model.to_bytes(), "after {:?}", op);
        }
    }
}
