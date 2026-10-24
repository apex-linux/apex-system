#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]

//! Introspection carries no dictionary keys or string values, so clients generated from the
//! XML read them from its comments; these tests hold the comments to the code.

use apex_dbus::{
    BootVerdict, ChangeKind, Channel, Deployment, ErrorKind, INTROSPECTION_XML, State,
};

/// The lines after `heading` in the header comment, up to the next blank line.
fn header_table(heading: &str) -> Vec<&'static str> {
    let (_, rest) = INTROSPECTION_XML
        .split_once(heading)
        .unwrap_or_else(|| panic!("the header comment has {heading:?}"));
    rest.lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .collect()
}

/// The comment right before `element`, e.g. `<property name="State"`.
fn comment_before(element: &str) -> &'static str {
    let (before, _) = INTROSPECTION_XML
        .split_once(element)
        .unwrap_or_else(|| panic!("the XML declares {element}"));
    let (_, comment) = before.rsplit_once("<!--").expect("a comment precedes it");
    comment
}

fn quoted(text: &str) -> Vec<&str> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .filter(|word| !word.is_empty())
        .collect()
}

fn strings<T: Copy>(all: &[T], as_str: fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|value| as_str(*value)).collect()
}

#[test]
fn the_deployment_key_table_matches_the_dictionary() {
    let documented: Vec<(String, String)> = header_table("Deployment dictionaries")
        .into_iter()
        .filter(|line| line.trim_start().starts_with('"'))
        .map(|line| {
            let mut words = line.split_whitespace();
            let key = words.next().expect("a key").trim_matches('"').to_owned();
            let signature = words.next().expect("a signature").to_owned();
            (key, signature)
        })
        .collect();
    let code: Vec<(String, String)> = Deployment::KEYS
        .iter()
        .map(|(key, signature)| ((*key).to_owned(), signature.to_string()))
        .collect();
    assert_eq!(documented, code);
}

#[test]
fn the_error_table_matches_the_error_kinds() {
    let documented: Vec<&str> = header_table("Errors io.github.apex_linux.Apex.Update1.Error")
        .into_iter()
        .map(|line| line.split_whitespace().next().expect("a name"))
        .collect();
    assert_eq!(documented, strings(&ErrorKind::ALL, ErrorKind::as_str));
}

#[test]
fn the_documented_values_match_the_enums() {
    assert_eq!(
        quoted(comment_before(r#"<property name="State""#)),
        strings(State::ALL, State::as_str)
    );
    assert_eq!(
        quoted(comment_before(r#"<property name="Channel""#)),
        strings(Channel::ALL, Channel::as_str)
    );
    assert_eq!(
        quoted(comment_before(r#"<method name="GetChanges""#)),
        strings(ChangeKind::ALL, ChangeKind::as_str)
    );
    let (_, verdicts) = comment_before(r#"<method name="ReportBoot""#)
        .split_once("verdict:")
        .expect("the verdict table");
    let verdicts: Vec<&str> = verdicts
        .lines()
        .filter(|line| line.trim_start().starts_with('"'))
        .filter_map(|line| quoted(line).first().copied())
        .collect();
    assert_eq!(verdicts, strings(BootVerdict::ALL, BootVerdict::as_str));
}
