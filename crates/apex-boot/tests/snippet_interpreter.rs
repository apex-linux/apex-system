//! Runs the real `data/07_apex-menu.cfg` and `data/08_apex-bootcount.cfg` through an interpreter
//! of just the GRUB script they use; anything else panics, so neither outgrows these tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use apex_boot::grubenv::GrubEnv;
use apex_boot::snippet::{
    BootChoice, BootEntry, MENU_SNIPPET, Menu, SNIPPET, grub_boot, grub_menu,
};

#[derive(Debug)]
enum Operand {
    Var(String),
    Literal(String),
}

#[derive(Debug)]
enum Cond {
    NonEmpty(Operand),
    Eq(Operand, Operand),
    Ne(Operand, Operand),
}

#[derive(Debug)]
enum Stmt {
    If {
        branches: Vec<(Cond, Vec<Stmt>)>,
        otherwise: Vec<Stmt>,
    },
    Set(String, String),
    SaveEnv(Vec<String>),
}

fn operand(token: &str) -> Operand {
    let unquoted = token
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(token);
    if let Some(name) = unquoted
        .strip_prefix("${")
        .and_then(|t| t.strip_suffix('}'))
    {
        Operand::Var(name.to_owned())
    } else {
        assert!(!unquoted.contains('$'), "unsupported expansion {token}");
        Operand::Literal(unquoted.to_owned())
    }
}

fn condition(line: &str, keyword: &str) -> Cond {
    let test = line
        .strip_prefix(keyword)
        .and_then(|l| l.strip_suffix("; then"))
        .unwrap_or_else(|| panic!("unsupported line {line:?}"))
        .trim();
    let inner = test
        .strip_prefix("[ ")
        .and_then(|t| t.strip_suffix(" ]"))
        .unwrap_or_else(|| panic!("unsupported test {test:?}"));
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    match tokens.as_slice() {
        ["-n", value] => Cond::NonEmpty(operand(value)),
        [a, "=", b] => Cond::Eq(operand(a), operand(b)),
        [a, "!=", b] => Cond::Ne(operand(a), operand(b)),
        _ => panic!("unsupported test {test:?}"),
    }
}

fn block(lines: &[&str], i: &mut usize) -> Vec<Stmt> {
    let mut stmts = Vec::new();
    while let Some(line) = lines.get(*i) {
        if line.starts_with("elif ") || *line == "else" || *line == "fi" {
            break;
        }
        *i += 1;
        if line.starts_with("if ") {
            let mut branches = vec![(condition(line, "if "), block(lines, i))];
            let mut otherwise = Vec::new();
            loop {
                let next = *lines.get(*i).expect("missing fi");
                *i += 1;
                if next.starts_with("elif ") {
                    branches.push((condition(next, "elif "), block(lines, i)));
                } else if next == "else" {
                    otherwise = block(lines, i);
                } else if next == "fi" {
                    break;
                } else {
                    panic!("unexpected {next:?}");
                }
            }
            stmts.push(Stmt::If {
                branches,
                otherwise,
            });
        } else if let Some(assignment) = line.strip_prefix("set ") {
            let (name, value) = assignment.split_once('=').expect("set without =");
            assert!(
                !value.contains(['"', '$', ' ']),
                "unsupported value {value:?}"
            );
            stmts.push(Stmt::Set(name.to_owned(), value.to_owned()));
        } else if let Some(names) = line.strip_prefix("save_env ") {
            stmts.push(Stmt::SaveEnv(
                names.split_whitespace().map(str::to_owned).collect(),
            ));
        } else {
            panic!("unsupported statement {line:?}");
        }
    }
    stmts
}

fn parse(script: &str) -> Vec<Stmt> {
    let lines: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    for line in &lines {
        assert!(
            !line.contains('#'),
            "inline comments are not supported: {line:?}"
        );
    }
    let mut i = 0;
    let stmts = block(&lines, &mut i);
    assert_eq!(i, lines.len(), "unparsed line {:?}", lines.get(i));
    stmts
}

struct Grub<'a> {
    vars: HashMap<String, String>,
    env: &'a mut GrubEnv,
}

impl Grub<'_> {
    fn value(&self, operand: &Operand) -> String {
        match operand {
            Operand::Var(name) => self.vars.get(name).cloned().unwrap_or_default(),
            Operand::Literal(value) => value.clone(),
        }
    }

    fn test(&self, cond: &Cond) -> bool {
        match cond {
            Cond::NonEmpty(v) => !self.value(v).is_empty(),
            Cond::Eq(a, b) => self.value(a) == self.value(b),
            Cond::Ne(a, b) => self.value(a) != self.value(b),
        }
    }

    fn run(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::If {
                    branches,
                    otherwise,
                } => {
                    let taken = branches.iter().find(|(cond, _)| self.test(cond));
                    match taken {
                        Some((_, body)) => self.run(body),
                        None => self.run(otherwise),
                    }
                }
                Stmt::Set(name, value) => {
                    self.vars.insert(name.clone(), value.clone());
                }
                // grub_cmd_save_env: set the variable if it is defined, delete it otherwise.
                Stmt::SaveEnv(names) => {
                    for name in names {
                        match self.vars.get(name) {
                            Some(value) => self.env.set(name, value).unwrap(),
                            None => self.env.unset(name).unwrap(),
                        }
                    }
                }
            }
        }
    }
}

fn menu_entry(value: Option<&String>, variable: &str) -> Option<BootEntry> {
    match value.map(String::as_str) {
        None => None,
        Some("1") => Some(BootEntry::Previous),
        Some(other) => panic!("unexpected {variable}={other}"),
    }
}

/// GRUB's variables after `script`, which starts from what `load_env` read.
fn run_script(script: &str, env: &mut GrubEnv) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for (name, value) in env.vars() {
        vars.insert(
            String::from_utf8(name.to_vec()).unwrap(),
            String::from_utf8(value.to_vec()).unwrap(),
        );
    }
    let script = parse(script);
    let mut grub = Grub { vars, env };
    grub.run(&script);
    grub.vars
}

fn boot_real_snippet(env: &mut GrubEnv) -> BootChoice {
    let vars = run_script(SNIPPET, env);
    BootChoice {
        entry: menu_entry(vars.get("default"), "default").unwrap_or(BootEntry::Default),
        fallback: menu_entry(vars.get("fallback"), "fallback"),
    }
}

fn real_menu(env: &mut GrubEnv) -> Menu {
    let vars = run_script(MENU_SNIPPET, env);
    let style = vars.get("timeout_style").map(String::as_str);
    Menu {
        hidden: match style {
            Some("hidden") => true,
            Some("menu") => false,
            other => panic!("timeout_style {other:?}"),
        },
        timeout: vars["timeout"].parse().unwrap(),
    }
}

const TARGET: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";

#[test]
fn snippet_matches_the_model_for_every_relevant_state() {
    let targets = [None, Some(""), Some(TARGET)];
    let counters = [
        None,
        Some(""),
        Some("3"),
        Some("2"),
        Some("1"),
        Some("0"),
        Some("-1"),
        Some("4"),
        Some("03"),
        Some("x"),
    ];
    let successes = [None, Some(""), Some("0"), Some("1"), Some("2")];
    let mut cases = 0;
    for target in targets {
        for counter in counters {
            for success in successes {
                let mut env = GrubEnv::new();
                env.set("saved_entry", "unrelated").unwrap();
                for (name, value) in [
                    ("apex_target", target),
                    ("apex_counter", counter),
                    ("apex_ok", success),
                ] {
                    if let Some(value) = value {
                        env.set(name, value).unwrap();
                    }
                }
                let mut real_env = env.clone();
                let real_entry = boot_real_snippet(&mut real_env);
                let mut model_env = env.clone();
                let model_entry = grub_boot(&mut model_env).unwrap();
                let state = format!("{target:?} {counter:?} {success:?}");
                assert_eq!(real_entry, model_entry, "{state}");
                assert_eq!(real_env.to_bytes(), model_env.to_bytes(), "{state}");
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 150);
}

#[test]
fn snippet_counts_three_boots_then_falls_back() {
    let mut env = GrubEnv::new();
    env.set("apex_target", TARGET).unwrap();
    env.set("apex_counter", "3").unwrap();
    env.set("apex_ok", "0").unwrap();
    let choices: Vec<(BootEntry, Option<BootEntry>)> = (0..5)
        .map(|_| {
            let choice = boot_real_snippet(&mut env);
            (choice.entry, choice.fallback)
        })
        .collect();
    let counting = (BootEntry::Default, Some(BootEntry::Previous));
    let fallen_back = (BootEntry::Previous, None);
    assert_eq!(
        choices,
        [counting, counting, counting, fallen_back, fallen_back]
    );
    assert_eq!(env.get("apex_counter"), Some(&b"-1"[..]));
    assert_eq!(env.get("fallback"), None);
    assert_eq!(env.get("default"), None);
}

#[test]
fn a_foreign_boot_success_does_not_confirm_an_armed_boot() {
    let mut env = GrubEnv::new();
    env.set("apex_target", TARGET).unwrap();
    env.set("apex_counter", "2").unwrap();
    env.set("apex_ok", "0").unwrap();
    env.set("boot_success", "1").unwrap();
    env.set("boot_counter", "3").unwrap();
    let choice = boot_real_snippet(&mut env);
    assert_eq!(choice.entry, BootEntry::Default);
    assert_eq!(env.get("apex_counter"), Some(&b"1"[..]));
    assert_eq!(env.get("boot_success"), Some(&b"1"[..]));
    assert_eq!(env.get("boot_counter"), Some(&b"3"[..]));
}

#[test]
fn menu_snippet_matches_the_model_and_writes_nothing() {
    for ok in [None, Some(""), Some("0"), Some("1"), Some("2"), Some("01")] {
        let mut env = GrubEnv::new();
        env.set("menu_show_once_timeout", "30").unwrap();
        if let Some(ok) = ok {
            env.set("apex_ok", ok).unwrap();
        }
        let before = env.to_bytes();
        let menu = real_menu(&mut env);
        assert_eq!(menu, grub_menu(&env), "{ok:?}");
        assert_eq!(env.to_bytes(), before, "no save_env: {ok:?}");
        let confirmed = ok == Some("1");
        assert_eq!(
            menu,
            Menu {
                hidden: confirmed,
                timeout: if confirmed { 1 } else { 5 }
            }
        );
    }
}

#[test]
fn a_green_boot_hides_the_next_menu_and_any_other_boot_shows_it() {
    let mut env = GrubEnv::new();
    assert!(!real_menu(&mut env).hidden, "a new install shows the menu");
    boot_real_snippet(&mut env);
    env.set("apex_ok", "1").unwrap();
    assert!(
        real_menu(&mut env).hidden,
        "after apex-boot-assess confirmed"
    );
    boot_real_snippet(&mut env);
    assert!(
        !real_menu(&mut env).hidden,
        "a boot nobody confirmed, a crash or a red one, shows it again"
    );
}

#[test]
fn grub_accepts_the_syntax() {
    let Some(checker) = ["/usr/bin/grub2-script-check", "/usr/bin/grub-script-check"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        eprintln!("skipped: no grub-script-check installed");
        return;
    };
    for name in ["data/07_apex-menu.cfg", "data/08_apex-bootcount.cfg"] {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let output = Command::new(checker).arg(&file).output().unwrap();
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
