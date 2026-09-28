//! The contract XML itself: well-formed, structurally what the crate assumes, and saying the
//! same thing as the crate's constants and enums.
//!
//! Several value sets live only in the XML's comments (states, channels, verdicts, change
//! kinds, error kinds, deployment keys, polkit actions). They are read from there, so a change
//! to the XML fails here until the crate follows.

#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]

use std::collections::BTreeSet;

use apex_dbus::{
    BUS_NAME, BootVerdict, CHANGE_FIELDS, Change, ChangeKind, Channel, Deployment, ERROR_PREFIX,
    ErrorKind, INTERFACE, INTROSPECTION_XML, OBJECT_PATH, PolkitAction, State,
};
use quick_xml::{Reader, XmlVersion, events::Event};

/// A comment and the element it documents.
#[derive(Debug)]
struct Documented {
    /// Local name of the element (`node`, `property`, `method`, …).
    element: String,
    /// Its `name` attribute, if any.
    name: Option<String>,
    /// The comments directly before it, joined.
    comment: String,
}

/// What the well-formedness pass saw.
#[derive(Debug, Default)]
struct Scan {
    documented: Vec<Documented>,
    doctype: Option<String>,
    root_elements: usize,
}

/// Reads the whole document with the checks XML 1.0 well-formedness needs beyond what a
/// D-Bus introspection parser does: matching end tags, no `--` inside comments, valid
/// attributes and references, exactly one root element, nothing but whitespace outside it.
fn scan(xml: &str) -> Result<Scan, String> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_comments = true;
    config.check_end_names = true;

    let mut scan = Scan::default();
    let mut depth = 0_usize;
    let mut pending = String::new();
    loop {
        let position = reader.buffer_position();
        let event = reader
            .read_event()
            .map_err(|error| format!("byte {position}: {error}"))?;
        match event {
            Event::Eof => break,
            Event::DocType(doctype) => scan.doctype = Some(doctype.trim().to_owned()),
            Event::Comment(comment) => {
                pending.push_str(&comment);
                pending.push('\n');
            }
            Event::Start(ref element) | Event::Empty(ref element) => {
                if depth == 0 {
                    scan.root_elements += 1;
                }
                let mut name = None;
                for attribute in element.attributes().with_checks(true) {
                    let attribute =
                        attribute.map_err(|error| format!("byte {position}: {error}"))?;
                    // The contract has no XML declaration, so it is (implicitly) XML 1.0.
                    let value = attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|error| format!("byte {position}: {error}"))?;
                    if attribute.key.as_ref() == "name" {
                        name = Some(value.into_owned());
                    }
                }
                scan.documented.push(Documented {
                    element: element.local_name().as_ref().to_owned(),
                    name,
                    comment: std::mem::take(&mut pending),
                });
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Text(text) => {
                if depth == 0 && !text.trim().is_empty() {
                    return Err(format!("byte {position}: text outside the root element"));
                }
            }
            Event::GeneralRef(reference) => {
                let name: &str = &reference;
                let known = matches!(name, "lt" | "gt" | "amp" | "apos" | "quot")
                    || reference.is_char_ref();
                if !known {
                    return Err(format!("byte {position}: undefined entity &{name};"));
                }
            }
            Event::CData(_) | Event::Decl(_) | Event::PI(_) => {}
        }
    }
    if depth != 0 {
        return Err(format!("{depth} element(s) left open"));
    }
    if scan.root_elements != 1 {
        return Err(format!("{} root elements", scan.root_elements));
    }
    Ok(scan)
}

fn contract() -> Scan {
    scan(INTROSPECTION_XML).unwrap_or_else(|error| panic!("contract XML is ill-formed: {error}"))
}

/// The comment documenting `element` named `name` (`None` for the root node's header).
fn comment_of<'s>(scan: &'s Scan, element: &str, name: Option<&str>) -> &'s str {
    scan.documented
        .iter()
        .find(|documented| documented.element == element && documented.name.as_deref() == name)
        .map(|documented| documented.comment.as_str())
        .filter(|comment| !comment.trim().is_empty())
        .unwrap_or_else(|| panic!("no comment documents <{element} name={name:?}>"))
}

/// Every double-quoted string in `text`, deduplicated.
fn quoted(text: &str) -> BTreeSet<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The text of `text` between the first `start` and the next `end` after it.
fn between<'t>(text: &'t str, start: &str, end: &str) -> &'t str {
    let (_, rest) = text
        .split_once(start)
        .unwrap_or_else(|| panic!("{start:?} not found in {text:?}"));
    rest.split_once(end)
        .unwrap_or_else(|| panic!("{end:?} does not follow {start:?}"))
        .0
}

fn strings<T: Copy>(values: &[T], as_str: fn(T) -> &'static str) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| as_str(*value).to_owned())
        .collect()
}

#[test]
fn contract_xml_is_well_formed() {
    let scan = contract();
    assert_eq!(
        scan.doctype.as_deref(),
        Some(
            "node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n \
             \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\""
        ),
        "the D-Bus introspection DOCTYPE"
    );
    // zbus's own parser must accept it too, without skipping unknown elements.
    let (_, warnings) = zbus_xml::Node::from_reader_with_warnings(INTROSPECTION_XML.as_bytes())
        .expect("zbus_xml parses the contract");
    assert!(warnings.is_empty(), "ignored elements: {warnings:?}");
}

#[test]
fn well_formedness_check_rejects_broken_documents() {
    let broken = [
        ("<node><!-- a -- b --></node>", "double hyphen in a comment"),
        ("<node><interface></node></interface>", "mismatched end tag"),
        ("<node><interface></node>", "unclosed element"),
        ("<node/><node/>", "two roots"),
        ("<node name=\"a\" name=\"b\"/>", "duplicate attribute"),
        ("<node>&nbsp;</node>", "undefined entity"),
        ("<node/>stray", "text after the root"),
    ];
    for (xml, why) in broken {
        assert!(
            scan(xml).is_err(),
            "accepted a document with a {why}: {xml}"
        );
    }
    assert!(scan("<node><!-- fine - comment --><interface name=\"a.b\"/></node>").is_ok());
}

#[test]
fn interface_shape_is_what_the_crate_assumes() {
    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    assert!(node.nodes().is_empty(), "no child nodes");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };
    assert_eq!(interface.name().as_str(), INTERFACE);
    assert!(
        interface.signals().is_empty(),
        "the contract has no signals of its own"
    );
    for property in interface.properties() {
        assert_eq!(
            property.access(),
            zbus_xml::PropertyAccess::Read,
            "{} must be read-only: every mutation is a polkit-guarded method",
            property.name()
        );
    }
}

#[test]
fn header_names_match_constants() {
    let scan = contract();
    let header = comment_of(&scan, "node", None);
    let value_after = |label: &str| {
        header
            .lines()
            .find_map(|line| line.trim().strip_prefix(label))
            .map_or_else(|| panic!("header has no {label:?} line"), str::trim)
    };
    assert_eq!(value_after("Object path:"), OBJECT_PATH);
    let bus = value_after("Bus:");
    assert_eq!(bus.rsplit(' ').next(), Some(BUS_NAME), "{bus}");
    let error_names = header
        .split_whitespace()
        .find_map(|word| word.strip_suffix(".<Kind>,"))
        .expect("header names the error prefix as <prefix>.<Kind>");
    assert_eq!(error_names, ERROR_PREFIX);
}

#[test]
fn error_kinds_match_the_header() {
    let scan = contract();
    let header = comment_of(&scan, "node", None);
    let (_, list) = header
        .split_once("Kind one of:")
        .expect("header lists the error kinds");
    let (list, _) = list
        .split_once('.')
        .expect("the list ends with a full stop");
    let from_xml: Vec<&str> = list.split(',').map(str::trim).collect();
    let from_crate: Vec<&str> = ErrorKind::ALL.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(from_xml, from_crate);
}

#[test]
fn deployment_keys_match_the_header() {
    let scan = contract();
    let header = comment_of(&scan, "node", None);
    let (_, block) = header
        .split_once("use these keys, all optional:")
        .expect("header lists the deployment keys");
    let (block, _) = block
        .split_once("An empty dictionary")
        .expect("the key list ends before the empty-dictionary rule");
    let from_xml: Vec<(String, String)> = block
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let rest = line.strip_prefix('"')?;
            let (key, rest) = rest.split_once('"')?;
            let signature = rest.split_whitespace().next()?;
            Some((key.to_owned(), signature.to_owned()))
        })
        .collect();
    let from_crate: Vec<(String, String)> = Deployment::KEYS
        .iter()
        .map(|(key, signature)| ((*key).to_owned(), signature.to_string()))
        .collect();
    assert_eq!(from_xml, from_crate);
}

#[test]
fn polkit_actions_match_the_header() {
    let scan = contract();
    let header = comment_of(&scan, "node", None);
    let from_xml: Vec<(String, String)> = header
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("io.github.sekirokenjii.apex.update."))
        .map(|line| {
            let mut words = line.split_whitespace();
            let id = words.next().unwrap_or_default().to_owned();
            let default = words.next().unwrap_or_default().to_owned();
            (id, default)
        })
        .collect();
    let from_crate: Vec<(String, String)> = PolkitAction::ALL
        .iter()
        .map(|action| {
            (
                action.id().to_owned(),
                action.allow_active().as_str().to_owned(),
            )
        })
        .collect();
    assert_eq!(from_xml, from_crate);
}

#[test]
fn string_values_match_the_member_comments() {
    let scan = contract();
    let cases = [
        (
            "State",
            quoted(comment_of(&scan, "property", Some("State"))),
            strings(State::ALL, State::as_str),
        ),
        (
            "Channel",
            quoted(comment_of(&scan, "property", Some("Channel"))),
            strings(Channel::ALL, Channel::as_str),
        ),
        (
            "ChangeKind",
            // Only the list after "kind one of": the rest of the comment also quotes "", the
            // empty EVR.
            quoted(between(
                comment_of(&scan, "method", Some("GetChanges")),
                "kind one of",
                ";",
            )),
            strings(ChangeKind::ALL, ChangeKind::as_str),
        ),
        (
            "BootVerdict",
            quoted(comment_of(&scan, "method", Some("ReportBoot"))),
            strings(BootVerdict::ALL, BootVerdict::as_str),
        ),
    ];
    for (what, from_xml, from_crate) in cases {
        assert_eq!(
            from_xml, from_crate,
            "apex_dbus::{what} differs from the XML comment"
        );
    }
}

#[test]
fn change_tuple_matches_the_get_changes_comment() {
    let scan = contract();
    let comment = comment_of(&scan, "method", Some("GetChanges"));
    let tuple = between(between(comment, "Entries:", "kind one of"), "(", ")");
    let fields: Vec<&str> = tuple.split(',').map(str::trim).collect();
    assert_eq!(fields, CHANGE_FIELDS, "field names and order of Change");
    assert!(
        comment.contains("Sorted by (name, arch)"),
        "the order clients rely on: {comment}"
    );
    for rule in [
        "from_evr is \"\" for added packages",
        "to_evr is \"\" for removed ones",
    ] {
        assert!(comment.contains(rule), "missing {rule:?}: {comment}");
    }

    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };
    let method = interface
        .methods()
        .iter()
        .find(|method| method.name() == "GetChanges")
        .expect("GetChanges");
    let [out] = method.args() else {
        panic!("GetChanges has exactly one argument");
    };
    assert_eq!(out.direction(), Some(zbus_xml::ArgDirection::Out));
    assert_eq!(
        out.ty().to_string(),
        <Vec<Change> as zbus::zvariant::Type>::SIGNATURE.to_string()
    );
}

#[test]
fn polkit_guards_match_the_xml() {
    let scan = contract();
    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };

    // Every method is classified: guarded by an action, or one of the unguarded ones.
    let unguarded: BTreeSet<String> = interface
        .methods()
        .iter()
        .map(|method| method.name().to_string())
        .filter(|name| PolkitAction::guarding(name).is_none())
        .collect();
    let expected: BTreeSet<String> = ["Ping", "GetChanges", "Automatic", "ReportBoot"]
        .map(str::to_owned)
        .into();
    assert_eq!(unguarded, expected);

    // A method comment that names its action ("Polkit: switch-channel (auth_admin)").
    for documented in &scan.documented {
        let (Some(name), Some((_, rest))) = (
            documented.name.as_deref(),
            documented.comment.split_once("Polkit: "),
        ) else {
            continue;
        };
        let suffix = rest.split_whitespace().next().unwrap_or_default();
        let action = PolkitAction::guarding(name).unwrap_or_else(|| panic!("{name} unguarded"));
        assert_eq!(
            action.id().rsplit('.').next(),
            Some(suffix.trim_end_matches('.')),
            "{name}"
        );
    }

    // The header's "(also guards Method, …)" after an action.
    let header = comment_of(&scan, "node", None);
    let mut extra = 0;
    for line in header.lines().map(str::trim) {
        let Some(id) = line
            .split_whitespace()
            .next()
            .filter(|id| id.starts_with("io.github.sekirokenjii.apex.update."))
        else {
            continue;
        };
        let action = PolkitAction::from_id(id).unwrap_or_else(|| panic!("unknown action {id}"));
        if let Some((_, guards)) = line.split_once("(also guards ") {
            for member in guards.trim_end_matches(')').split(',').map(str::trim) {
                assert_eq!(PolkitAction::guarding(member), Some(action), "{member}");
                extra += 1;
            }
        }
    }
    assert_eq!(
        extra, 1,
        "the XML names AcknowledgeHealthFailure under discard"
    );
}

/// One member of the `Update1` proxy trait, as written in `src/proxy.rs`.
#[derive(Debug)]
struct Declared {
    dbus_name: String,
    property: bool,
    params: Vec<String>,
}

/// Reads the proxy trait's declarations from its source.
///
/// Argument names never reach the wire, so the wire tests cannot see the proxy's; this reads
/// them from the source instead. Every trait item carries an explicit `name = "…"`, which keeps
/// the parsing to plain text.
fn proxy_declarations() -> Vec<Declared> {
    let source = include_str!("../src/proxy.rs");
    let (_, body) = source
        .split_once("pub trait Update1 {")
        .expect("src/proxy.rs declares `pub trait Update1`");
    let (body, _) = body.split_once("\n}\n").expect("the trait body ends");
    let functions = body.matches("    fn ").count();

    let mut declared = Vec::new();
    let mut rest = body;
    while let Some((_, after)) = rest.split_once("#[zbus(") {
        let (attribute, after) = after.split_once(")]").expect("attribute closes");
        let dbus_name = attribute
            .split_once("name = \"")
            .and_then(|(_, name)| name.split('"').next())
            .unwrap_or_else(|| panic!("#[zbus({attribute})] has no explicit name"))
            .to_owned();
        let property = attribute.split(',').any(|part| part.trim() == "property");
        let (_, after) = after.split_once("fn ").expect("attribute precedes a fn");
        let (_, after) = after.split_once('(').expect("parameter list opens");
        let (params, after) = after.split_once(')').expect("parameter list closes");
        let params = params
            .split(',')
            .map(str::trim)
            .filter(|param| !param.is_empty() && *param != "&self")
            .map(|param| {
                param
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
            .collect();
        declared.push(Declared {
            dbus_name,
            property,
            params,
        });
        rest = after;
    }
    assert_eq!(
        declared.len(),
        functions,
        "every trait fn needs #[zbus(name = …)]"
    );
    declared
}

#[test]
fn proxy_declares_the_contract_names() {
    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };
    let xml_methods: Vec<(String, Vec<String>)> = interface
        .methods()
        .iter()
        .map(|method| {
            let inputs = method
                .args()
                .iter()
                .filter(|arg| arg.direction() != Some(zbus_xml::ArgDirection::Out))
                .map(|arg| arg.name().unwrap_or_default().to_owned())
                .collect();
            (method.name().to_string(), inputs)
        })
        .collect();
    let xml_properties: BTreeSet<String> = interface
        .properties()
        .iter()
        .map(|property| property.name().to_string())
        .collect();

    let declared = proxy_declarations();
    let mut proxy_methods: Vec<(String, Vec<String>)> = declared
        .iter()
        .filter(|item| !item.property)
        .map(|item| (item.dbus_name.clone(), item.params.clone()))
        .collect();
    let proxy_properties: BTreeSet<String> = declared
        .iter()
        .filter(|item| item.property)
        .map(|item| item.dbus_name.clone())
        .collect();

    let mut xml_methods = xml_methods;
    xml_methods.sort();
    proxy_methods.sort();
    assert_eq!(
        proxy_methods, xml_methods,
        "method names and in-argument names"
    );
    assert_eq!(proxy_properties, xml_properties, "property names");
    assert!(
        declared
            .iter()
            .filter(|item| item.property)
            .all(|item| item.params.is_empty()),
        "properties are read-only: no setters"
    );
}
