#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; clippy's allow-unwrap-in-tests does not cover them"
)]

use std::fmt::Write as _;

use apex_bootc::{Progress, ProgressDecoder, Stage};

fn stream() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/progress/upgrade-download-only.jsonl"
    ))
    .unwrap()
}

fn render(updates: &[Progress]) -> String {
    let mut out = String::new();
    for p in updates {
        let fraction = p
            .fraction
            .map_or_else(|| "-".to_owned(), |f| format!("{f:.3}"));
        let overall = p
            .overall()
            .map_or_else(|| "-".to_owned(), |f| format!("{f:.3}"));
        let bytes = p
            .bytes
            .map_or_else(String::new, |b| format!(" bytes={}/{}", b.done, b.total));
        writeln!(
            out,
            "{:<9} fraction={fraction:<5} overall={overall:<5}{bytes} label={:?}",
            p.stage.as_str(),
            p.label
        )
        .unwrap();
    }
    out
}

#[test]
fn full_stream_snapshot() {
    let mut decoder = ProgressDecoder::new();
    let mut updates = decoder.push(&stream());
    updates.extend(decoder.finish());
    // 21 lines: Start, one non-JSON line and one unknown event type are skipped.
    assert_eq!(updates.len(), 18);
    insta::assert_snapshot!(render(&updates));
}

#[test]
fn overall_progress_never_goes_backwards_across_a_retry() {
    let mut decoder = ProgressDecoder::new();
    let updates = decoder.push(&stream());
    let retry = updates
        .iter()
        .position(|p| p.stage == Stage::Retrying)
        .unwrap();
    let mut last = 0.0;
    for (i, p) in updates.iter().enumerate() {
        match p.overall() {
            Some(o) => {
                assert!(o >= last, "{o} < {last} at {p:?}");
                last = o;
            }
            None => assert!(
                p.stage == Stage::Retrying || (p.stage == Stage::Importing && i < retry),
                "{p:?}"
            ),
        }
    }
    assert!((last - 1.0).abs() < f64::EPSILON);

    let pulled: Vec<_> = updates
        .iter()
        .filter(|p| p.stage == Stage::Pulling)
        .map(|p| p.bytes.unwrap())
        .collect();
    let after_retry = updates[retry + 1].bytes.unwrap();
    assert_eq!(after_retry.done, 0);
    assert!(after_retry.total < pulled[0].total);
    assert!(pulled[2].done > after_retry.done);
}

#[test]
fn byte_at_a_time_delivery_gives_the_same_updates() {
    let bytes = stream();
    let mut whole = ProgressDecoder::new();
    let expected = whole.push(&bytes);
    let mut decoder = ProgressDecoder::new();
    let mut got = Vec::new();
    for b in &bytes {
        got.extend(decoder.push(std::slice::from_ref(b)));
    }
    got.extend(decoder.finish());
    assert_eq!(got, expected);
}
