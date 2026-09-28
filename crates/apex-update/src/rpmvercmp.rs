//! A statement-by-statement port of rpm 6.1.0 `rpmio/rpmvercmp.cc`, oddities included, so it
//! matches `rpm.vercmp` (the rpm test vectors are in `tests/rpmvercmp_vectors.rs`).

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

// rpm's locale-independent `risdigit`, `risalpha` and `risalnum`.
const fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

const fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

const fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// NUL past the end, as C string access sees it.
fn peek(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn c_str(s: &str) -> &[u8] {
    let bytes = s.as_bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// A total preorder: strings that differ only in their separators are equal.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    let a = c_str(a);
    let b = c_str(b);

    if a == b {
        return Ordering::Equal;
    }

    // Cursors named as in rpm's C code.
    let mut one = 0;
    let mut two = 0;

    while peek(a, one) != 0 || peek(b, two) != 0 {
        while one < a.len() && !is_alnum(a[one]) && a[one] != b'~' && a[one] != b'^' {
            one += 1;
        }
        while two < b.len() && !is_alnum(b[two]) && b[two] != b'~' && b[two] != b'^' {
            two += 1;
        }
        let c1 = peek(a, one);
        let c2 = peek(b, two);

        // `~` sorts before everything, even the end of the string.
        if c1 == b'~' || c2 == b'~' {
            if c1 != b'~' {
                return Ordering::Greater;
            }
            if c2 != b'~' {
                return Ordering::Less;
            }
            one += 1;
            two += 1;
            continue;
        }

        // `^` sorts after the end of the string but before any other segment.
        if c1 == b'^' || c2 == b'^' {
            if c1 == 0 {
                return Ordering::Less;
            }
            if c2 == 0 {
                return Ordering::Greater;
            }
            if c1 != b'^' {
                return Ordering::Greater;
            }
            if c2 != b'^' {
                return Ordering::Less;
            }
            one += 1;
            two += 1;
            continue;
        }

        if c1 == 0 || c2 == 0 {
            break;
        }

        // `a` decides the segment type; `b` is walked with the same character class.
        let is_num = is_digit(c1);
        let class: fn(u8) -> bool = if is_num { is_digit } else { is_alpha };
        let mut end1 = one;
        while end1 < a.len() && class(a[end1]) {
            end1 += 1;
        }
        let mut end2 = two;
        while end2 < b.len() && class(b[end2]) {
            end2 += 1;
        }

        // Unreachable, since `a` has a segment here; rpm returns -1 ("arbitrary").
        if one == end1 {
            return Ordering::Less;
        }

        // Different segment types: a numeric segment is always newer than an alpha one.
        if two == end2 {
            return if is_num {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        let mut seg1 = &a[one..end1];
        let mut seg2 = &b[two..end2];
        if is_num {
            // Digit strings compare without parsing, so they cannot overflow.
            seg1 = strip_leading_zeros(seg1);
            seg2 = strip_leading_zeros(seg2);
            match seg1.len().cmp(&seg2.len()) {
                Ordering::Equal => {}
                unequal => return unequal,
            }
        }

        match seg1.cmp(seg2) {
            Ordering::Equal => {}
            unequal => return unequal,
        }

        one = end1;
        two = end2;
    }

    let c1 = peek(a, one);
    let c2 = peek(b, two);
    if c1 == 0 && c2 == 0 {
        return Ordering::Equal;
    }

    if c1 == 0 {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

fn strip_leading_zeros(digits: &[u8]) -> &[u8] {
    let first_non_zero = digits
        .iter()
        .position(|&d| d != b'0')
        .unwrap_or(digits.len());
    &digits[first_non_zero..]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvrRef<'a> {
    pub epoch: u32,
    pub version: &'a str,
    pub release: &'a str,
}

impl EvrRef<'_> {
    /// rpm's `rpmverCmp` (`rpmio/rpmver.cc`) without its missing epoch and missing release
    /// cases, which manifests never have.
    pub fn rpm_cmp(&self, other: &EvrRef<'_>) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| rpmvercmp(self.version, other.version))
            .then_with(|| rpmvercmp(self.release, other.release))
    }

    pub fn to_evr(self) -> Evr {
        Evr {
            epoch: self.epoch,
            version: self.version.to_owned(),
            release: self.release.to_owned(),
        }
    }
}

impl fmt::Display for EvrRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}:", self.epoch)?;
        }
        write!(f, "{}-{}", self.version, self.release)
    }
}

/// No `Ord`: rpm finds `1.0` and `1_0` equal, which would contradict the derived `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evr {
    pub epoch: u32,
    pub version: String,
    pub release: String,
}

impl Evr {
    pub fn new(epoch: u32, version: impl Into<String>, release: impl Into<String>) -> Self {
        Self {
            epoch,
            version: version.into(),
            release: release.into(),
        }
    }

    pub fn as_evr_ref(&self) -> EvrRef<'_> {
        EvrRef {
            epoch: self.epoch,
            version: &self.version,
            release: &self.release,
        }
    }

    pub fn rpm_cmp(&self, other: &Evr) -> Ordering {
        self.as_evr_ref().rpm_cmp(&other.as_evr_ref())
    }
}

impl fmt::Display for Evr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_evr_ref().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn identical_strings_are_equal() {
        assert_eq!(rpmvercmp("", ""), Equal);
        assert_eq!(rpmvercmp("1.0", "1.0"), Equal);
    }

    #[test]
    fn empty_string_is_older_than_any_segment_and_newer_than_tilde() {
        assert_eq!(rpmvercmp("", "1"), Less);
        assert_eq!(rpmvercmp("", "a"), Less);
        assert_eq!(rpmvercmp("", "^"), Less);
        assert_eq!(rpmvercmp("", "~"), Greater);
        assert_eq!(rpmvercmp("", "._+"), Equal);
    }

    #[test]
    fn long_numbers_do_not_overflow() {
        let big = "123456789012345678901234567890";
        let bigger = "123456789012345678901234567891";
        assert_eq!(rpmvercmp(big, bigger), Less);
        assert_eq!(rpmvercmp(&format!("000{big}"), big), Equal);
        assert_eq!(rpmvercmp("99999999999999999999999", "1"), Greater);
    }

    #[test]
    fn numeric_segment_beats_alpha_segment_from_either_side() {
        assert_eq!(rpmvercmp("1.1", "1.a"), Greater);
        assert_eq!(rpmvercmp("1.a", "1.1"), Less);
    }

    #[test]
    fn nul_byte_ends_the_string_like_in_c() {
        assert_eq!(rpmvercmp("1.0\0zzz", "1.0"), Equal);
        assert_eq!(rpmvercmp("1.0", "1.0\x001"), Equal);
        assert_eq!(rpmvercmp("\0", ""), Equal);
    }

    #[test]
    fn trailing_separators_do_not_matter() {
        assert_eq!(rpmvercmp("1.0", "1.0."), Equal);
        assert_eq!(rpmvercmp("1.0.", "1.0"), Equal);
        assert_eq!(rpmvercmp("1..0", "1.0"), Equal);
    }

    #[test]
    fn evr_compares_epoch_then_version_then_release() {
        let a = Evr::new(0, "51.1", "1.fc45");
        let newer_epoch = Evr::new(1, "1.0", "1.fc45");
        let newer_version = Evr::new(0, "51.2", "0.fc45");
        let newer_release = Evr::new(0, "51.1", "2.fc45");
        assert_eq!(a.rpm_cmp(&newer_epoch), Less);
        assert_eq!(a.rpm_cmp(&newer_version), Less);
        assert_eq!(a.rpm_cmp(&newer_release), Less);
        assert_eq!(newer_release.rpm_cmp(&a), Greater);
        assert_eq!(a.rpm_cmp(&a.clone()), Equal);
        assert_eq!(
            Evr::new(0, "1.0", "1").rpm_cmp(&Evr::new(0, "1_0", "1")),
            Equal
        );
    }

    #[test]
    fn evr_display_omits_zero_epoch() {
        assert_eq!(
            Evr::new(0, "7.2.9", "200.fc45").to_string(),
            "7.2.9-200.fc45"
        );
        assert_eq!(Evr::new(2, "1.0", "3").to_string(), "2:1.0-3");
        assert_eq!(
            Evr::new(2, "1.0", "3").as_evr_ref().to_evr(),
            Evr::new(2, "1.0", "3")
        );
    }

    fn version_string() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                4 => "[0-9]{1,3}",
                3 => "[a-zA-Z]{1,3}",
                2 => Just(".".to_owned()),
                1 => Just("~".to_owned()),
                1 => Just("^".to_owned()),
                1 => Just("_".to_owned()),
                1 => Just("+".to_owned()),
                1 => Just("0".to_owned()),
                1 => Just("é".to_owned()),
            ],
            0..8,
        )
        .prop_map(|parts| parts.concat())
    }

    proptest! {
        #[test]
        fn reflexive(a in version_string()) {
            prop_assert_eq!(rpmvercmp(&a, &a), Equal);
        }

        #[test]
        fn antisymmetric(a in version_string(), b in version_string()) {
            prop_assert_eq!(rpmvercmp(&a, &b), rpmvercmp(&b, &a).reverse());
        }

        #[test]
        fn transitive(a in version_string(), b in version_string(), c in version_string()) {
            let ab = rpmvercmp(&a, &b);
            let bc = rpmvercmp(&b, &c);
            let ac = rpmvercmp(&a, &c);
            if ab != Greater && bc != Greater {
                prop_assert_ne!(ac, Greater, "{} <= {} <= {} but {} > {}", a, b, c, a, c);
            }
            if ab == Equal && bc == Equal {
                prop_assert_eq!(ac, Equal);
            }
            if ab == Less && bc == Less {
                prop_assert_eq!(ac, Less);
            }
        }

        #[test]
        fn sorting_yields_a_consistent_chain(mut v in proptest::collection::vec(version_string(), 0..24)) {
            v.sort_by(|x, y| rpmvercmp(x, y));
            for i in 0..v.len() {
                for j in i + 1..v.len() {
                    prop_assert_ne!(rpmvercmp(&v[i], &v[j]), Greater);
                }
            }
        }

        #[test]
        fn evr_order_is_antisymmetric(
            e1 in 0u32..3, v1 in version_string(), r1 in version_string(),
            e2 in 0u32..3, v2 in version_string(), r2 in version_string(),
        ) {
            let a = Evr::new(e1, v1, r1);
            let b = Evr::new(e2, v2, r2);
            prop_assert_eq!(a.rpm_cmp(&b), b.rpm_cmp(&a).reverse());
        }
    }
}
