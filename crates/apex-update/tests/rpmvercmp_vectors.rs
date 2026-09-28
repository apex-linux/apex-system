//! The vectors are copied in file order from rpm `tests/rpmvercmp.at` at `rpm-6.1.0-release`
//! (GPL-2.0-or-later), with its commented-out oddities block.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::cmp::Ordering;

use apex_update::rpmvercmp;
use proptest::prelude::*;

const RPM_VECTORS: &[(&str, &str, i8)] = &[
    ("1.0", "1.0", 0),
    ("1.0", "2.0", -1),
    ("2.0", "1.0", 1),
    ("2.0.1", "2.0.1", 0),
    ("2.0", "2.0.1", -1),
    ("2.0.1", "2.0", 1),
    ("2.0.1a", "2.0.1a", 0),
    ("2.0.1a", "2.0.1", 1),
    ("2.0.1", "2.0.1a", -1),
    ("5.5p1", "5.5p1", 0),
    ("5.5p1", "5.5p2", -1),
    ("5.5p2", "5.5p1", 1),
    ("5.5p10", "5.5p10", 0),
    ("5.5p1", "5.5p10", -1),
    ("5.5p10", "5.5p1", 1),
    ("10xyz", "10.1xyz", -1),
    ("10.1xyz", "10xyz", 1),
    ("xyz10", "xyz10", 0),
    ("xyz10", "xyz10.1", -1),
    ("xyz10.1", "xyz10", 1),
    ("xyz.4", "xyz.4", 0),
    ("xyz.4", "8", -1),
    ("8", "xyz.4", 1),
    ("xyz.4", "2", -1),
    ("2", "xyz.4", 1),
    ("5.5p2", "5.6p1", -1),
    ("5.6p1", "5.5p2", 1),
    ("5.6p1", "6.5p1", -1),
    ("6.5p1", "5.6p1", 1),
    ("6.0.rc1", "6.0", 1),
    ("6.0", "6.0.rc1", -1),
    ("10b2", "10a1", 1),
    ("10a2", "10b2", -1),
    ("1.0aa", "1.0aa", 0),
    ("1.0a", "1.0aa", -1),
    ("1.0aa", "1.0a", 1),
    ("10.0001", "10.0001", 0),
    ("10.0001", "10.1", 0),
    ("10.1", "10.0001", 0),
    ("10.0001", "10.0039", -1),
    ("10.0039", "10.0001", 1),
    ("4.999.9", "5.0", -1),
    ("5.0", "4.999.9", 1),
    ("20101121", "20101121", 0),
    ("20101121", "20101122", -1),
    ("20101122", "20101121", 1),
    ("2_0", "2_0", 0),
    ("2.0", "2_0", 0),
    ("2_0", "2.0", 0),
    // RhBug:178798 case
    ("a", "a", 0),
    ("a+", "a+", 0),
    ("a+", "a_", 0),
    ("a_", "a+", 0),
    ("+a", "+a", 0),
    ("+a", "_a", 0),
    ("_a", "+a", 0),
    ("+_", "+_", 0),
    ("_+", "+_", 0),
    ("_+", "_+", 0),
    ("+", "_", 0),
    ("_", "+", 0),
    ("1.0~rc1", "1.0~rc1", 0),
    ("1.0~rc1", "1.0", -1),
    ("1.0", "1.0~rc1", 1),
    ("1.0~rc1", "1.0~rc2", -1),
    ("1.0~rc2", "1.0~rc1", 1),
    ("1.0~rc1~git123", "1.0~rc1~git123", 0),
    ("1.0~rc1~git123", "1.0~rc1", -1),
    ("1.0~rc1", "1.0~rc1~git123", 1),
    ("1.0^", "1.0^", 0),
    ("1.0^", "1.0", 1),
    ("1.0", "1.0^", -1),
    ("1.0^git1", "1.0^git1", 0),
    ("1.0^git1", "1.0", 1),
    ("1.0", "1.0^git1", -1),
    ("1.0^git1", "1.0^git2", -1),
    ("1.0^git2", "1.0^git1", 1),
    ("1.0^git1", "1.01", -1),
    ("1.01", "1.0^git1", 1),
    ("1.0^20160101", "1.0^20160101", 0),
    ("1.0^20160101", "1.0.1", -1),
    ("1.0.1", "1.0^20160101", 1),
    ("1.0^20160101^git1", "1.0^20160101^git1", 0),
    ("1.0^20160102", "1.0^20160101^git1", 1),
    ("1.0^20160101^git1", "1.0^20160102", -1),
    ("1.0~rc1^git1", "1.0~rc1^git1", 0),
    ("1.0~rc1^git1", "1.0~rc1", 1),
    ("1.0~rc1", "1.0~rc1^git1", -1),
    ("1.0^git1~pre", "1.0^git1~pre", 0),
    ("1.0^git1", "1.0^git1~pre", 1),
    ("1.0^git1~pre", "1.0^git1", -1),
];

const RPM_ODDITIES: &[(&str, &str, i8)] = &[
    // RhBug:811992 case
    ("1b.fc17", "1b.fc17", 0),
    ("1b.fc17", "1.fc17", -1),
    ("1.fc17", "1b.fc17", 1),
    ("1g.fc17", "1g.fc17", 0),
    ("1g.fc17", "1.fc17", 1),
    ("1.fc17", "1g.fc17", -1),
    // Every non-ASCII character is a separator.
    ("1.1.α", "1.1.α", 0),
    ("1.1.α", "1.1.β", 0),
    ("1.1.β", "1.1.α", 0),
    ("1.1.αα", "1.1.α", 0),
    ("1.1.α", "1.1.ββ", 0),
    ("1.1.ββ", "1.1.αα", 0),
];

fn expected(n: i8) -> Ordering {
    n.cmp(&0)
}

#[test]
fn rpm_test_vectors() {
    assert_eq!(
        RPM_VECTORS.len(),
        91,
        "every RPMVERCMP line of rpmvercmp.at"
    );
    for &(a, b, want) in RPM_VECTORS {
        assert_eq!(rpmvercmp(a, b), expected(want), "rpmvercmp({a:?}, {b:?})");
    }
}

#[test]
fn rpm_documented_oddities() {
    for &(a, b, want) in RPM_ODDITIES {
        assert_eq!(rpmvercmp(a, b), expected(want), "rpmvercmp({a:?}, {b:?})");
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Token {
    Tilde,
    End,
    Caret,
    Alpha(Vec<u8>),
    Num(Vec<u8>),
}

fn tokens(s: &str) -> Vec<Token> {
    let bytes = s.as_bytes();
    let bytes = &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())];
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'~' {
            out.push(Token::Tilde);
            i += 1;
        } else if c == b'^' {
            out.push(Token::Caret);
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &bytes[start..i];
            let zeros = digits.iter().take_while(|&&d| d == b'0').count();
            out.push(Token::Num(digits[zeros..].to_vec()));
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            out.push(Token::Alpha(bytes[start..i].to_vec()));
        } else {
            i += 1;
        }
    }
    out.push(Token::End);
    out
}

fn num_cmp(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn model(a: &str, b: &str) -> Ordering {
    let (ta, tb) = (tokens(a), tokens(b));
    for (x, y) in ta.iter().zip(tb.iter()) {
        let ord = match (x, y) {
            (Token::Num(p), Token::Num(q)) => num_cmp(p, q),
            _ => x.cmp(y),
        };
        if ord != Ordering::Equal || *x == Token::End {
            return ord;
        }
    }
    Ordering::Equal
}

#[test]
fn model_agrees_on_the_rpm_vectors() {
    for &(a, b, want) in RPM_VECTORS.iter().chain(RPM_ODDITIES) {
        assert_eq!(model(a, b), expected(want), "model({a:?}, {b:?})");
    }
}

fn version_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => "[0-9]{1,4}",
            3 => "[a-zA-Z]{1,3}",
            3 => Just(".".to_owned()),
            1 => Just("~".to_owned()),
            1 => Just("^".to_owned()),
            1 => "[-_+]",
            1 => Just("00".to_owned()),
            1 => Just("ß".to_owned()),
        ],
        0..10,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn port_matches_the_token_model(a in version_string(), b in version_string()) {
        prop_assert_eq!(rpmvercmp(&a, &b), model(&a, &b), "{:?} vs {:?}", a, b);
    }

    #[test]
    fn port_matches_the_token_model_on_arbitrary_text(a in ".{0,12}", b in ".{0,12}") {
        prop_assert_eq!(rpmvercmp(&a, &b), model(&a, &b), "{:?} vs {:?}", a, b);
    }
}
