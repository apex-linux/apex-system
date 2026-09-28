//! RPM version comparison: a faithful port of rpm's `rpmvercmp` plus EVR ordering.
//!
//! The port follows `rpmio/rpmvercmp.cc` from rpm 6.1.0 (2026-08-20) statement by
//! statement, so its behaviour matches `rpm --eval '%{lua: print(rpm.vercmp(a, b))}'`,
//! including the documented oddities:
//!
//! - only ASCII letters and digits form segments; every other byte, including every
//!   byte of a non-ASCII character, is a separator, and runs of separators are equal to
//!   each other (`2.0` = `2_0`, `1.1.α` = `1.1.β`);
//! - `~` sorts before everything, even the end of the string (`1.0~rc1 < 1.0`);
//! - `^` sorts after the end of the string but before any other segment
//!   (`1.0 < 1.0^git1 < 1.0.1`);
//! - a numeric segment is newer than an alphabetic one, and numbers compare by value
//!   without overflow (leading zeros are ignored, then the longer digit run wins);
//! - a C string ends at its first NUL byte, so this port ignores everything from the
//!   first NUL onwards.
//!
//! [`EvrRef::rpm_cmp`] follows `rpmverCmp` from `rpmio/rpmver.cc`: epoch, then version,
//! then release. Apex manifests always carry a numeric epoch and a release, so the
//! "missing epoch" and "missing release" cases of `rpmverCmp` do not arise here.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Locale-independent `risdigit`.
const fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Locale-independent `risalpha`.
const fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

/// Locale-independent `risalnum`.
const fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// The byte at `i`, or NUL past the end, mirroring C string access.
fn peek(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// The prefix before the first NUL byte, which is where a C string would end.
fn c_str(s: &str) -> &[u8] {
    let bytes = s.as_bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// Compares two version (or release) strings exactly like rpm's `rpmvercmp`.
///
/// Returns [`Ordering::Greater`] when `a` is newer, [`Ordering::Less`] when `b` is newer
/// and [`Ordering::Equal`] when rpm considers them the same version. The relation is a
/// total preorder: strings that differ only in their separators compare equal.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    let a = c_str(a);
    let b = c_str(b);

    // Easy comparison to see if versions are identical.
    if a == b {
        return Ordering::Equal;
    }

    // `one` and `two` are the cursors of the C code; `peek` returns NUL at the end.
    let mut one = 0;
    let mut two = 0;

    // Loop through each version segment of a and b and compare them.
    while peek(a, one) != 0 || peek(b, two) != 0 {
        while one < a.len() && !is_alnum(a[one]) && a[one] != b'~' && a[one] != b'^' {
            one += 1;
        }
        while two < b.len() && !is_alnum(b[two]) && b[two] != b'~' && b[two] != b'^' {
            two += 1;
        }
        let c1 = peek(a, one);
        let c2 = peek(b, two);

        // Handle the tilde separator, it sorts before everything else.
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

        // Handle the caret separator. Same idea as tilde, except that if one of the
        // strings ends (base version), the other is considered the higher version.
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

        // If we ran to the end of either, we are finished with the loop.
        if c1 == 0 || c2 == 0 {
            break;
        }

        // Grab the first completely alpha or completely numeric segment. The segment
        // type is decided by `a`; `b` is walked with the same character class.
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

        // This cannot happen, as we previously tested to make sure that the first
        // string has a non-empty segment (rpm returns -1, "arbitrary").
        if one == end1 {
            return Ordering::Less;
        }

        // The two segments are different types: one numeric, the other alpha (empty
        // here). Numeric segments are always newer than alpha segments.
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
            // Throw away any leading zeros, then whichever number has more digits wins.
            // Comparing digit strings instead of parsing them cannot overflow.
            seg1 = strip_leading_zeros(seg1);
            seg2 = strip_leading_zeros(seg2);
            match seg1.len().cmp(&seg2.len()) {
                Ordering::Equal => {}
                unequal => return unequal,
            }
        }

        // `strcmp` of the two segments decides unless they are equal, in which case
        // there might be more segments to compare.
        match seg1.cmp(seg2) {
            Ordering::Equal => {}
            unequal => return unequal,
        }

        one = end1;
        two = end2;
    }

    // All segments compared identically but the separators were different.
    let c1 = peek(a, one);
    let c2 = peek(b, two);
    if c1 == 0 && c2 == 0 {
        return Ordering::Equal;
    }

    // Whichever version still has characters left over wins.
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

/// A borrowed epoch, version and release, the unit rpm compares packages by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvrRef<'a> {
    /// Package epoch; 0 when the package has none.
    pub epoch: u32,
    /// Upstream version, e.g. `51.1`.
    pub version: &'a str,
    /// Distribution release, e.g. `1.fc45`.
    pub release: &'a str,
}

impl EvrRef<'_> {
    /// Orders two EVRs like rpm's `rpmverCmp`: the numeric epoch first, then the
    /// version and the release with [`rpmvercmp`].
    pub fn rpm_cmp(&self, other: &EvrRef<'_>) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| rpmvercmp(self.version, other.version))
            .then_with(|| rpmvercmp(self.release, other.release))
    }

    /// An owned copy.
    pub fn to_evr(self) -> Evr {
        Evr {
            epoch: self.epoch,
            version: self.version.to_owned(),
            release: self.release.to_owned(),
        }
    }
}

impl fmt::Display for EvrRef<'_> {
    /// Formats as `[epoch:]version-release`, omitting a zero epoch like `rpm -q` does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}:", self.epoch)?;
        }
        write!(f, "{}-{}", self.version, self.release)
    }
}

/// An owned epoch, version and release.
///
/// `Evr` deliberately implements neither `Ord` nor rpm-aware equality: two textually
/// different EVRs can be equal to rpm (`1.0` and `1_0`), which would make `Ord`
/// inconsistent with the derived `Eq`. Use [`Evr::rpm_cmp`] to order them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evr {
    /// Package epoch; 0 when the package has none.
    pub epoch: u32,
    /// Upstream version.
    pub version: String,
    /// Distribution release.
    pub release: String,
}

impl Evr {
    /// Builds an EVR from its parts.
    pub fn new(epoch: u32, version: impl Into<String>, release: impl Into<String>) -> Self {
        Self {
            epoch,
            version: version.into(),
            release: release.into(),
        }
    }

    /// A borrowed view.
    pub fn as_evr_ref(&self) -> EvrRef<'_> {
        EvrRef {
            epoch: self.epoch,
            version: &self.version,
            release: &self.release,
        }
    }

    /// Orders two EVRs like rpm; see [`EvrRef::rpm_cmp`].
    pub fn rpm_cmp(&self, other: &Evr) -> Ordering {
        self.as_evr_ref().rpm_cmp(&other.as_evr_ref())
    }
}

impl fmt::Display for Evr {
    /// Formats as `[epoch:]version-release`, omitting a zero epoch like `rpm -q` does.
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
        // Separators alone are nothing.
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
        // rpm-equal but textually different.
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

    /// Version-like strings drawn from the characters that matter to rpmvercmp.
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
