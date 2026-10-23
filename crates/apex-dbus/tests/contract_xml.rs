#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]

use std::collections::BTreeSet;

use apex_dbus::{Change, INTERFACE, INTROSPECTION_XML, PolkitAction};
use quick_xml::{Reader, XmlVersion, events::Event};

#[derive(Debug, Default)]
struct Scan {
    doctype: Option<String>,
    root_elements: usize,
}

/// The XML 1.0 well-formedness checks that a D-Bus introspection parser skips.
fn scan(xml: &str) -> Result<Scan, String> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_comments = true;
    config.check_end_names = true;

    let mut scan = Scan::default();
    let mut depth = 0_usize;
    loop {
        let position = reader.buffer_position();
        let event = reader
            .read_event()
            .map_err(|error| format!("byte {position}: {error}"))?;
        match event {
            Event::Eof => break,
            Event::DocType(doctype) => scan.doctype = Some(doctype.trim().to_owned()),
            Event::Start(ref element) | Event::Empty(ref element) => {
                if depth == 0 {
                    scan.root_elements += 1;
                }
                for attribute in element.attributes().with_checks(true) {
                    let attribute =
                        attribute.map_err(|error| format!("byte {position}: {error}"))?;
                    // The contract has no XML declaration, so it is (implicitly) XML 1.0.
                    attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|error| format!("byte {position}: {error}"))?;
                }
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
            Event::Comment(_) | Event::CData(_) | Event::Decl(_) | Event::PI(_) => {}
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
        interface.signals().next().is_none(),
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
fn get_changes_returns_the_change_array() {
    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };
    let method = interface
        .methods()
        .find(|method| method.name() == "GetChanges")
        .expect("GetChanges");
    let [out] = method.args() else {
        panic!("GetChanges has exactly one argument");
    };
    assert_eq!(out.direction(), Some(zbus_xml::ArgDirection::Out));
    assert_eq!(
        out.ty().to_string(),
        <Vec<Change> as zbus::Type>::SIGNATURE.to_string()
    );
}

#[test]
fn polkit_guards_match_the_xml() {
    let node = zbus_xml::Node::try_from(INTROSPECTION_XML).expect("parses");
    let [interface] = node.interfaces() else {
        panic!("exactly one interface expected");
    };
    let unguarded: BTreeSet<String> = interface
        .methods()
        .map(|method| method.name().to_string())
        .filter(|name| PolkitAction::guarding(name).is_none())
        .collect();
    let expected: BTreeSet<String> = ["Ping", "GetChanges", "Automatic", "ReportBoot"]
        .map(str::to_owned)
        .into();
    assert_eq!(unguarded, expected);
}

#[derive(Debug)]
struct Declared {
    dbus_name: String,
    property: bool,
    params: Vec<String>,
}

/// Argument names never reach the wire, so they are read from the source; an explicit
/// `name = "…"` on every trait item keeps that parsing to plain text.
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
