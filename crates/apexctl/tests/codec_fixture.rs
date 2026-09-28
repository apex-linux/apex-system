//! The real codec dump of this laptop's ALC294 (`/proc/asound/card1/codec#0`, Ubuntu
//! 7.0.0-31, 2026-09-23, `dump_coef` off) parses into what the hardware docs describe.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use apexctl::codec;
use apexctl::hw::audio::{AudioSnapshot, render};

const FIXTURE: &str = include_str!("fixtures/alc294-m7400qc-codec0.txt");

#[test]
fn header_and_pins_match_the_hardware_notes() {
    let dump = codec::parse(FIXTURE).expect("the real dump parses");
    assert_eq!(dump.codec, "Realtek ALC294");
    assert_eq!(dump.address, Some(0));
    assert_eq!(dump.vendor_id, 0x10ec_0294);
    assert_eq!(dump.subsystem_id, 0x1043_1ab2);
    assert_eq!(dump.revision_id, 0x0010_0004);

    // docs/hardware/AUDIO.md: speaker on 0x14 fed by DAC 0x02, EAPD capable.
    let speaker = dump.node(0x14).expect("node 0x14");
    assert_eq!(
        speaker.pin_default.as_ref().map(|p| p.value),
        Some(0x9017_0110)
    );
    assert_eq!(
        speaker.pin_default.as_ref().map(|p| p.description.as_str()),
        Some("[Fixed] Speaker at Int N/A")
    );
    assert_eq!(speaker.pin_ctls.as_ref().map(|c| c.value), Some(0x40));
    assert_eq!(speaker.eapd.as_ref().map(|e| e.value), Some(0x2));
    assert_eq!(speaker.connections, [0x02]);

    // Headphone jack 0x21 selects DAC 0x03.
    let headphone = dump.node(0x21).expect("node 0x21");
    assert_eq!(
        headphone.pin_default.as_ref().map(|p| p.value),
        Some(0x0421_1020)
    );
    assert_eq!(headphone.connections, [0x02, 0x03]);
    assert_eq!(headphone.selected_connection, Some(0x03));
    let ctls = headphone.pin_ctls.as_ref().expect("pin-ctls");
    assert_eq!(
        (ctls.value, ctls.flags.as_slice()),
        (0xc0, &["OUT".to_owned(), "HP".to_owned()][..])
    );

    // Pin 0x13 prints "Pin-ctls: 0x00:" with no flags.
    let unused = dump.node(0x13).expect("node 0x13");
    assert!(unused.pin_ctls.as_ref().expect("pin-ctls").flags.is_empty());

    // The COEF widget: 130 slots, not dumped.
    let widget = dump.node(0x20).expect("node 0x20");
    assert_eq!(widget.kind, "Vendor Defined Widget");
    assert_eq!(widget.ncoeff, Some(130));
    assert!(widget.coefs.is_empty());

    assert_eq!(dump.pins().count(), 14);
    assert_eq!(dump.nodes.len(), 36);
}

#[test]
fn parsed_dump_snapshot() {
    let dump = codec::parse(FIXTURE).expect("the real dump parses");
    insta::assert_json_snapshot!(dump);
}

#[test]
fn human_summary_snapshot() {
    let snapshot = AudioSnapshot {
        source: "tests/fixtures/alc294-m7400qc-codec0.txt".to_owned(),
        card: Some(1),
        codec: codec::parse(FIXTURE).expect("the real dump parses"),
        hwdep: Some("hwC1D0".to_owned()),
        init_verbs: Some(vec![
            "0x20 0x500 0x001b".to_owned(),
            "0x20 0x400 0x4e4b".to_owned(),
        ]),
    };
    insta::assert_snapshot!(render(&snapshot));
}
