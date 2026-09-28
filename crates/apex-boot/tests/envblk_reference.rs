//! Checks `apex_boot::grubenv` against a line-by-line transliteration of GRUB's
//! `grub-core/lib/envblk.c`: the functions below keep the C control flow, pointer
//! arithmetic and "broken" early returns, so any difference in behaviour shows up as a
//! property failure.
//!
//! The source is Fedora's GRUB, which Apex boots: rhboot/grub2, branches `fedora-45` and
//! `rhel-10-main` (identical in this file), fetched 2026-09-24 from
//! `https://raw.githubusercontent.com/rhboot/grub2/fedora-45/grub-core/lib/envblk.c`. Its
//! `grub_envblk_iterate` skips only lines that start with `#` (Ubuntu's GRUB, the
//! `grub-editenv` the host tests may run, also skips lines starting with `\n` or `\r`).
//! Its `grub_envblk_set`, `grub_envblk_delete` and `find_next_line` are the functions
//! transliterated below.

use apex_boot::grubenv::{CREATE_WARNING, EditError, GrubEnv, SIGNATURE};
use proptest::prelude::*;

/// `find_next_line`: may return a value past `pend`, exactly like the C pointer.
fn c_find_next_line(buf: &[u8], mut p: usize, pend: usize) -> usize {
    while p < pend {
        if buf[p] == b'\\' {
            p += 2;
        } else if buf[p] == b'\n' {
            break;
        } else {
            p += 1;
        }
    }
    p + 1
}

fn c_escaped_value_len(value: &[u8]) -> usize {
    value
        .iter()
        .map(|&c| if c == b'\\' || c == b'\n' { 2 } else { 1 })
        .sum()
}

/// `grub_envblk_open`: `size < sizeof (SIGNATURE)` or a mismatch fails. `sizeof` counts the
/// C string's NUL, so the size must exceed the signature's length.
fn c_open(buf: &[u8]) -> bool {
    buf.len() > SIGNATURE.len() && buf.starts_with(SIGNATURE)
}

/// Variables as GRUB's iterate hook sees them: (name, value) C strings, in block order.
type Vars = Vec<(Vec<u8>, Vec<u8>)>;

/// `grub_envblk_set`. Returns 0/1 like the C function.
fn c_set(buf: &mut [u8], name: &[u8], value: &[u8]) -> bool {
    let nl = name.len();
    let vl = c_escaped_value_len(value);
    let mut p = SIGNATURE.len();
    let pend = buf.len();

    // First, look at free space.
    let mut space = pend - 1;
    while buf[space] == b'#' {
        space -= 1;
    }
    if buf[space] != b'\n' {
        return false; // Broken.
    }
    space += 1;

    let mut found = false;
    while p + nl + 1 < space {
        if &buf[p..p + nl] == name && buf[p + nl] == b'=' {
            // Found the same name.
            p += nl + 1;
            // Check the length of the current value.
            let mut len = 0;
            while p + len < pend && buf[p + len] != b'\n' {
                if buf[p + len] == b'\\' {
                    len += 2;
                } else {
                    len += 1;
                }
            }
            if p + len >= pend {
                return false; // Broken.
            }
            if ((pend - space) as isize) < (vl as isize - len as isize) {
                return false; // No space.
            }
            if vl < len {
                // memmove (p + vl, p + len, pend - (p + len)); memset (space - (len - vl), '#', len - vl)
                buf.copy_within(p + len..pend, p + vl);
                buf[space - (len - vl)..space].fill(b'#');
            } else {
                // memmove (p + vl, p + len, pend - (p + vl));
                buf.copy_within(p + len..p + len + (pend - (p + vl)), p + vl);
            }
            found = true;
            break;
        }
        p = c_find_next_line(buf, p, pend);
    }

    if !found {
        // Append a new variable.
        if pend - space < nl + 1 + vl + 1 {
            return false; // No space.
        }
        buf[space..space + nl].copy_from_slice(name);
        p = space + nl;
        buf[p] = b'=';
        p += 1;
    }

    // Write the value.
    for &c in value {
        if c == b'\\' || c == b'\n' {
            buf[p] = b'\\';
            p += 1;
        }
        buf[p] = c;
        p += 1;
    }
    buf[p] = b'\n';
    true
}

/// `grub_envblk_delete`.
fn c_delete(buf: &mut [u8], name: &[u8]) {
    let nl = name.len();
    let mut p = SIGNATURE.len();
    let pend = buf.len();
    while p + nl + 1 < pend {
        if &buf[p..p + nl] == name && buf[p + nl] == b'=' {
            // Found.
            let mut len = nl + 1;
            while p + len < pend {
                if buf[p + len] == b'\n' {
                    break;
                } else if buf[p + len] == b'\\' {
                    len += 2;
                } else {
                    len += 1;
                }
            }
            if p + len >= pend {
                return; // Broken.
            }
            len += 1;
            buf.copy_within(p + len..pend, p);
            buf[pend - len..].fill(b'#');
            break;
        }
        p = c_find_next_line(buf, p, pend);
    }
}

/// `grub_envblk_iterate` with a hook that collects (name, value) as C strings. The flag is
/// true when the loop returned early on a broken entry.
fn c_iterate(buf: &[u8]) -> (Vars, bool) {
    fn c_string(bytes: &[u8]) -> Vec<u8> {
        bytes.iter().copied().take_while(|&b| b != 0).collect()
    }
    let mut vars = Vec::new();
    let mut p = SIGNATURE.len();
    let pend = buf.len();
    while p < pend {
        if buf[p] != b'#' {
            let name_start = p;
            while p < pend && buf[p] != b'=' {
                p += 1;
            }
            if p == pend {
                return (vars, true); // Broken.
            }
            let name_end = p;
            p += 1;
            let value_start = p;
            while p < pend {
                if buf[p] == b'\n' {
                    break;
                } else if buf[p] == b'\\' {
                    p += 2;
                } else {
                    p += 1;
                }
            }
            if p >= pend {
                return (vars, true); // Broken.
            }
            let mut value = Vec::new();
            let mut q = value_start;
            while buf[q] != b'\n' {
                if buf[q] == b'\\' {
                    q += 1;
                }
                value.push(buf[q]);
                q += 1;
            }
            vars.push((c_string(&buf[name_start..name_end]), c_string(&value)));
        }
        p = c_find_next_line(buf, p, pend);
    }
    (vars, false)
}

/// The block `grub2-editenv create` writes.
fn created_block() -> Vec<u8> {
    let mut buf = SIGNATURE.to_vec();
    buf.extend_from_slice(CREATE_WARNING);
    buf.resize(1024, b'#');
    buf
}

#[derive(Debug, Clone)]
enum Op {
    Set(&'static str, Vec<u8>),
    Unset(&'static str),
}

fn arb_op() -> impl Strategy<Value = Op> {
    // A few names, two of them prefixes of others, to exercise the name matching.
    let name = prop::sample::select(vec![
        "a",
        "ab",
        "b",
        "apex_counter",
        "apex_ok",
        "apex_target",
        "x_1",
    ]);
    // Values: anything but NUL, with backslashes, newlines and '#' made likely.
    let byte = prop_oneof![
        3 => prop::sample::select(vec![b'\\', b'\n', b'#', b'=', b'\r', b'a', b'0']),
        1 => 1u8..=255,
    ];
    let value = prop::collection::vec(byte, 0..90);
    prop_oneof![
        3 => (name.clone(), value).prop_map(|(n, v)| Op::Set(n, v)),
        1 => name.prop_map(Op::Unset),
    ]
}

/// A block body built from arbitrary lines, to fuzz the reader.
fn arb_block() -> impl Strategy<Value = Vec<u8>> {
    let byte = prop::sample::select(vec![
        b'a', b'b', b'=', b'\n', b'\\', b'#', b'\r', b'\0', b'x', 0xff,
    ]);
    (prop::collection::vec(byte, 0..120), 0usize..40).prop_map(|(body, padding)| {
        let mut buf = SIGNATURE.to_vec();
        buf.extend_from_slice(&body);
        buf.extend(std::iter::repeat_n(b'#', padding));
        buf
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Any sequence of edits yields byte-identical blocks, and the same successes and
    /// failures, in the model and in GRUB's code.
    #[test]
    fn edits_match_grub(ops in prop::collection::vec(arb_op(), 0..40)) {
        let mut c_buf = created_block();
        let mut model = GrubEnv::new();
        prop_assert_eq!(model.to_bytes(), c_buf.clone());
        for op in &ops {
            match op {
                Op::Set(name, value) => {
                    let c_ok = c_set(&mut c_buf, name.as_bytes(), value);
                    let model_result = model.set(name, value);
                    prop_assert_eq!(c_ok, model_result.is_ok(), "{:?} {:?}", op, model_result);
                    if let Err(err) = model_result {
                        prop_assert!(matches!(err, EditError::NoSpace { .. }), "{:?}", err);
                    }
                }
                Op::Unset(name) => {
                    c_delete(&mut c_buf, name.as_bytes());
                    model.unset(name).unwrap();
                }
            }
            prop_assert_eq!(model.to_bytes(), c_buf.clone(), "after {:?}", op);
        }
        // Reading back: same variables as GRUB's iterate, and an exact round trip.
        let parsed = GrubEnv::parse(&c_buf).unwrap();
        prop_assert_eq!(&parsed, &model);
        prop_assert_eq!(parsed.to_bytes(), c_buf.clone());
        let (c_vars, broken) = c_iterate(&c_buf);
        prop_assert!(!broken);
        let vars: Vars =
            parsed.vars().map(|(n, v)| (n.to_vec(), v.to_vec())).collect();
        prop_assert_eq!(vars, c_vars);
    }

    /// The reader never panics, accepts exactly the blocks GRUB reads completely (except a
    /// name containing a newline, which it rejects on purpose), sees the same variables,
    /// and reproduces every block it accepts byte for byte.
    #[test]
    fn reader_matches_grub(buf in arb_block()) {
        let parsed = GrubEnv::parse(&buf);
        if !c_open(&buf) {
            prop_assert!(parsed.is_err());
            return Ok(());
        }
        let (c_vars, broken) = c_iterate(&buf);
        match parsed {
            Ok(env) => {
                prop_assert!(!broken);
                let vars: Vars =
                    env.vars().map(|(n, v)| (n.to_vec(), v.to_vec())).collect();
                prop_assert_eq!(vars, c_vars);
                prop_assert_eq!(env.to_bytes(), buf);
            }
            Err(apex_boot::grubenv::ParseError::NewlineInName { .. }) => {}
            Err(err) => prop_assert!(broken, "rejected a block GRUB reads: {:?}", err),
        }
    }

    /// Edits on arbitrary readable blocks also match GRUB, including blocks GRUB cannot
    /// write to.
    #[test]
    fn edits_on_arbitrary_blocks_match_grub(buf in arb_block(), op in arb_op()) {
        let Ok(mut model) = GrubEnv::parse(&buf) else { return Ok(()); };
        let mut c_buf = buf.clone();
        match &op {
            Op::Set(name, value) => {
                let c_ok = c_set(&mut c_buf, name.as_bytes(), value);
                let result = model.set(name, value);
                prop_assert_eq!(c_ok, result.is_ok(), "{:?}", result);
            }
            Op::Unset(name) => {
                c_delete(&mut c_buf, name.as_bytes());
                model.unset(name).unwrap();
            }
        }
        prop_assert_eq!(model.to_bytes(), c_buf);
    }
}

#[test]
fn reference_agrees_on_the_fixed_cases() {
    // A value that shrinks and one that grows, in place.
    let mut buf = created_block();
    assert!(c_set(&mut buf, b"a", b"long value"));
    assert!(c_set(&mut buf, b"b", b"x"));
    assert!(c_set(&mut buf, b"a", b"s"));
    assert!(c_set(&mut buf, b"b", b"much longer\nvalue\\"));
    let mut model = GrubEnv::new();
    model.set("a", "long value").unwrap();
    model.set("b", "x").unwrap();
    model.set("a", "s").unwrap();
    model.set("b", "much longer\nvalue\\").unwrap();
    assert_eq!(model.to_bytes(), buf);
}
