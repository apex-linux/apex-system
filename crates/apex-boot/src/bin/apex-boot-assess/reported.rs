//! The reported checks: logged and shown by `--dry-run`, never part of the decision. Every
//! path is read below `--root`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use apex_boot::checks::grub_cfg::{GRUB_CFG_PATH, GrubCfgCurrentCheck};
use apex_boot::checks::names;
use apex_boot::{Check, CheckOutcome, CheckResult, FnCheck};
use apex_bootc::{APEX_STABLE, APEX_TESTING, ImageSignature};

use crate::cli::under;
use crate::ports::BootedImage;

/// A test keeps these equal to `hardware/m7400qc/profile.toml`.
pub(crate) mod m7400qc {
    pub(crate) const BATTERY: &str = "BAT0";
    pub(crate) const CHARGE_END_THRESHOLD: u32 = 80;
    pub(crate) const DGPU_PCI: &str = "0000:01:00.0";
    pub(crate) const FINGERPRINT_VENDOR: u32 = 0x04f3;
    pub(crate) const FINGERPRINT_PRODUCT: u32 = 0x0c6e;
    pub(crate) const CODEC_VENDOR_ID: u32 = 0x10ec_0294;
    pub(crate) const CODEC_SUBSYSTEM_ID: u32 = 0x1043_1ab2;
    /// Absent while the image is labelled `degraded=audio`.
    pub(crate) const PATCH_FIRMWARE: &str = "/usr/lib/firmware/apex/alc294-m7400qc.fw";
}

/// In the order of [`names::REPORTED`]; the image source comes from `bootc status`.
pub(crate) fn file_checks(root: &Path) -> Vec<Box<dyn Check>> {
    let check = |name: &'static str, run: fn(&Path) -> Result<(), String>| -> Box<dyn Check> {
        let root = root.to_path_buf();
        Box::new(FnCheck::new(name, move || run(&root)))
    };
    vec![
        check(names::AUDIO_INIT_VERBS, audio_init_verbs),
        check(names::CHARGE_THRESHOLD, charge_threshold),
        check(names::DGPU_RUNTIME_PM, dgpu_runtime_pm),
        check(names::FINGERPRINT_DEVICE, fingerprint_device),
        Box::new(GrubCfgCurrentCheck::new(under(root, GRUB_CFG_PATH))),
    ]
}

pub(crate) fn image_source(booted: &Result<BootedImage, String>) -> CheckResult {
    let outcome = match booted {
        Err(err) => Err(format!("bootc status failed: {err}")),
        Ok(booted) => evaluate_image_source(booted),
    };
    CheckResult {
        name: names::IMAGE_SOURCE.to_owned(),
        outcome: outcome.map_or_else(CheckOutcome::Fail, |()| CheckOutcome::Pass),
    }
}

fn evaluate_image_source(booted: &BootedImage) -> Result<(), String> {
    let Some(image) = booted.image.as_deref() else {
        return Err("bootc reports no booted container image".to_owned());
    };
    let mut problems = Vec::new();
    if booted.transport.as_deref() != Some("registry") {
        problems.push(format!(
            "transport is {}, not registry",
            booted.transport.as_deref().unwrap_or("unknown")
        ));
    }
    let expected = [APEX_STABLE.to_string(), APEX_TESTING.to_string()];
    if !expected.iter().any(|e| e == image) {
        problems.push(format!(
            "image {image} is neither {} nor {}",
            expected[0], expected[1]
        ));
    }
    if booted.signature != Some(ImageSignature::ContainerPolicy) {
        problems.push(format!(
            "signature mode is {}, not containerPolicy",
            match &booted.signature {
                None => "unset".to_owned(),
                Some(ImageSignature::ContainerPolicy) => "containerPolicy".to_owned(),
                Some(ImageSignature::Insecure) => "insecure".to_owned(),
                Some(ImageSignature::OstreeRemote(remote)) => format!("ostreeRemote {remote}"),
                Some(ImageSignature::Unknown) => "unknown".to_owned(),
            }
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HdaVerb {
    pub(crate) nid: u32,
    pub(crate) verb: u32,
    pub(crate) param: u32,
}

impl fmt::Display for HdaVerb {
    /// The kernel's `init_verbs` format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "0x{:02x} 0x{:03x} 0x{:04x}",
            self.nid, self.verb, self.param
        )
    }
}

/// As the kernel's `%i` reads it.
pub(crate) fn parse_number(token: &str) -> Option<u32> {
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()
    } else if token.len() > 1
        && let Some(octal) = token.strip_prefix('0')
    {
        u32::from_str_radix(octal, 8).ok()
    } else {
        token.parse().ok()
    }
}

/// The kernel reads a verb line with `"%i %i %i"`.
pub(crate) fn parse_verb_line(line: &str) -> Option<HdaVerb> {
    let mut tokens = line.split_whitespace().map(parse_number);
    Some(HdaVerb {
        nid: tokens.next()??,
        verb: tokens.next()??,
        param: tokens.next()??,
    })
}

pub(crate) fn patch_verbs(firmware: &str) -> Result<Vec<HdaVerb>, String> {
    let mut in_verbs = false;
    let mut verbs = Vec::new();
    for (index, line) in firmware.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_verbs = matches!(section, "verb" | "init_verbs");
        } else if in_verbs {
            verbs.push(
                parse_verb_line(line)
                    .ok_or_else(|| format!("line {}: not a verb: {line:?}", index + 1))?,
            );
        }
    }
    Ok(verbs)
}

pub(crate) fn sysfs_verbs(text: &str) -> Vec<HdaVerb> {
    text.lines().filter_map(parse_verb_line).collect()
}

pub(crate) fn missing_verbs(expected: &[HdaVerb], actual: &[HdaVerb]) -> Vec<HdaVerb> {
    expected
        .iter()
        .filter(|v| !actual.contains(v))
        .copied()
        .collect()
}

fn read_trimmed(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map(|s| s.trim().to_owned())
}

fn describe_read_error(path: &Path, err: &io::Error) -> String {
    format!("cannot read {}: {err}", path.display())
}

/// The kernel replays the patch firmware's verbs only if they are in the codec's `init_verbs`.
fn audio_init_verbs(root: &Path) -> Result<(), String> {
    let firmware_path = under(root, m7400qc::PATCH_FIRMWARE);
    let firmware = match fs::read_to_string(&firmware_path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "{} is not installed (the image ships audio degraded)",
                m7400qc::PATCH_FIRMWARE
            ));
        }
        Err(err) => return Err(describe_read_error(&firmware_path, &err)),
    };
    let expected = patch_verbs(&firmware)?;
    let codec = find_codec(root)?;
    let init_verbs_path = codec.join("init_verbs");
    let actual = sysfs_verbs(
        &fs::read_to_string(&init_verbs_path)
            .map_err(|err| describe_read_error(&init_verbs_path, &err))?,
    );
    let missing = missing_verbs(&expected, &actual);
    if missing.is_empty() {
        Ok(())
    } else {
        let list: Vec<String> = missing.iter().map(ToString::to_string).collect();
        Err(format!(
            "{} lacks {} of the {} patch verbs: {}",
            init_verbs_path.display(),
            missing.len(),
            expected.len(),
            list.join(", ")
        ))
    }
}

fn find_codec(root: &Path) -> Result<PathBuf, String> {
    let sound = under(root, "/sys/class/sound");
    let entries = fs::read_dir(&sound).map_err(|err| describe_read_error(&sound, &err))?;
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("hwC"))
        .map(|e| e.path())
        .filter(|dir| {
            let id = |file: &str| {
                read_trimmed(&dir.join(file))
                    .ok()
                    .and_then(|s| parse_number(&s))
            };
            id("vendor_id") == Some(m7400qc::CODEC_VENDOR_ID)
                && id("subsystem_id") == Some(m7400qc::CODEC_SUBSYSTEM_ID)
        })
        .collect();
    found.sort();
    found.into_iter().next().ok_or_else(|| {
        format!(
            "no HDA codec {:#010x} (subsystem {:#010x}) under {}",
            m7400qc::CODEC_VENDOR_ID,
            m7400qc::CODEC_SUBSYSTEM_ID,
            sound.display()
        )
    })
}

fn charge_threshold(root: &Path) -> Result<(), String> {
    let path = under(
        root,
        &format!(
            "/sys/class/power_supply/{}/charge_control_end_threshold",
            m7400qc::BATTERY
        ),
    );
    let value = read_trimmed(&path).map_err(|err| describe_read_error(&path, &err))?;
    match value.parse::<u32>() {
        Ok(threshold) if threshold == m7400qc::CHARGE_END_THRESHOLD => Ok(()),
        Ok(threshold) => Err(format!(
            "{} charge_control_end_threshold is {threshold}, expected {}",
            m7400qc::BATTERY,
            m7400qc::CHARGE_END_THRESHOLD
        )),
        Err(_) => Err(format!("{} holds {value:?}", path.display())),
    }
}

fn dgpu_runtime_pm(root: &Path) -> Result<(), String> {
    let path = under(
        root,
        &format!("/sys/bus/pci/devices/{}/power/control", m7400qc::DGPU_PCI),
    );
    match read_trimmed(&path) {
        Ok(value) if value == "auto" => Ok(()),
        Ok(value) => Err(format!(
            "dGPU {} runtime PM is {value:?}, expected \"auto\"",
            m7400qc::DGPU_PCI
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Err(format!("dGPU {} is not present", m7400qc::DGPU_PCI))
        }
        Err(err) => Err(describe_read_error(&path, &err)),
    }
}

fn fingerprint_device(root: &Path) -> Result<(), String> {
    let usb = under(root, "/sys/bus/usb/devices");
    let entries = fs::read_dir(&usb).map_err(|err| describe_read_error(&usb, &err))?;
    let hex = |path: PathBuf| {
        read_trimmed(&path)
            .ok()
            .and_then(|s| u32::from_str_radix(&s, 16).ok())
    };
    let present = entries.filter_map(Result::ok).any(|entry| {
        let dir = entry.path();
        hex(dir.join("idVendor")) == Some(m7400qc::FINGERPRINT_VENDOR)
            && hex(dir.join("idProduct")) == Some(m7400qc::FINGERPRINT_PRODUCT)
    });
    if present {
        Ok(())
    } else {
        Err(format!(
            "no USB device {:04x}:{:04x}",
            m7400qc::FINGERPRINT_VENDOR,
            m7400qc::FINGERPRINT_PRODUCT
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_boot::run_checks;
    use proptest::prelude::*;
    use std::fmt::Write as _;

    const FIRMWARE: &str = "\
[codec]
0x10ec0294 0x10431ab2 0

[model]
headset-mic

[verb]
# COEF 0x1b: set bit 10
0x20 0x500 0x1b
0x20 0x400 0x4e4b
0x20 0x500 0x0f
0x20 0x400 0x7774
";

    fn healthy_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = under(root.path(), path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(m7400qc::PATCH_FIRMWARE, FIRMWARE);
        write("/sys/class/sound/hwC0D0/vendor_id", "0x1002aa01\n");
        write("/sys/class/sound/hwC0D0/subsystem_id", "0x00aa0100\n");
        write("/sys/class/sound/hwC1D0/vendor_id", "0x10ec0294\n");
        write("/sys/class/sound/hwC1D0/subsystem_id", "0x10431ab2\n");
        write(
            "/sys/class/sound/hwC1D0/init_verbs",
            "0x20 0x500 0x001b\n0x20 0x400 0x4e4b\n0x20 0x500 0x000f\n0x20 0x400 0x7774\n",
        );
        write(
            "/sys/class/power_supply/BAT0/charge_control_end_threshold",
            "80\n",
        );
        write("/sys/bus/pci/devices/0000:01:00.0/power/control", "auto\n");
        write("/sys/bus/usb/devices/1-1/idVendor", "0bda\n");
        write("/sys/bus/usb/devices/1-1/idProduct", "0129\n");
        write("/sys/bus/usb/devices/3-4/idVendor", "04f3\n");
        write("/sys/bus/usb/devices/3-4/idProduct", "0c6e\n");
        write(
            GRUB_CFG_PATH,
            &format!("{}blscfg\n", apex_boot::snippet::SNIPPET),
        );
        root
    }

    fn outcomes(root: &Path) -> Vec<(String, CheckOutcome)> {
        run_checks(file_checks(root))
            .into_iter()
            .map(|r| (r.name, r.outcome))
            .collect()
    }

    #[test]
    fn a_healthy_machine_passes_in_the_documented_order() {
        let root = healthy_root();
        let results = outcomes(root.path());
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, names::REPORTED[..5]);
        assert!(results.iter().all(|(_, o)| o.is_pass()), "{results:?}");
    }

    #[test]
    fn each_check_explains_its_failure() {
        let root = healthy_root();
        let path = |p: &str| under(root.path(), p);
        fs::write(
            path("/sys/class/sound/hwC1D0/init_verbs"),
            "0x20 0x500 0x001b\n",
        )
        .unwrap();
        fs::write(
            path("/sys/class/power_supply/BAT0/charge_control_end_threshold"),
            "100\n",
        )
        .unwrap();
        fs::write(
            path("/sys/bus/pci/devices/0000:01:00.0/power/control"),
            "on\n",
        )
        .unwrap();
        fs::remove_dir_all(path("/sys/bus/usb/devices/3-4")).unwrap();
        fs::write(path(GRUB_CFG_PATH), "# apex-bootcount v0\nblscfg\n").unwrap();
        let reasons: Vec<String> = outcomes(root.path())
            .into_iter()
            .map(|(name, outcome)| format!("{name}: {outcome}"))
            .collect();
        let init_verbs = path("/sys/class/sound/hwC1D0/init_verbs");
        assert_eq!(
            reasons,
            [
                format!(
                    "audio-init-verbs: fail: {} lacks 3 of the 4 patch verbs: \
                     0x20 0x400 0x4e4b, 0x20 0x500 0x000f, 0x20 0x400 0x7774",
                    init_verbs.display()
                ),
                "charge-threshold: fail: BAT0 charge_control_end_threshold is 100, expected 80"
                    .to_owned(),
                "dgpu-runtime-pm: fail: dGPU 0000:01:00.0 runtime PM is \"on\", expected \"auto\""
                    .to_owned(),
                "fingerprint-device: fail: no USB device 04f3:0c6e".to_owned(),
                format!(
                    "grub-cfg-current: fail: grub.cfg contains \"# apex-bootcount v0\"; \
                     this image works with snippet versions {} to {}",
                    apex_boot::snippet::MIN_COMPATIBLE_VERSION,
                    apex_boot::snippet::MARKER_VERSION
                ),
            ]
        );
    }

    #[test]
    fn a_degraded_image_and_missing_hardware_are_named() {
        let root = tempfile::tempdir().unwrap();
        let reasons: Vec<String> = outcomes(root.path())
            .into_iter()
            .map(|(_, outcome)| outcome.to_string())
            .collect();
        assert_eq!(
            reasons[0],
            "fail: /usr/lib/firmware/apex/alc294-m7400qc.fw is not installed \
             (the image ships audio degraded)"
        );
        assert_eq!(reasons[2], "fail: dGPU 0000:01:00.0 is not present");
    }

    fn booted(image: &str, transport: &str, signature: Option<ImageSignature>) -> BootedImage {
        BootedImage {
            digest: None,
            image: Some(image.to_owned()),
            transport: Some(transport.to_owned()),
            signature,
            ..BootedImage::default()
        }
    }

    #[test]
    fn image_source_accepts_only_verified_apex_images() {
        for image in [
            "ghcr.io/apex-linux/apex:45",
            "ghcr.io/apex-linux/apex-testing:45",
        ] {
            let ok = image_source(&Ok(booted(
                image,
                "registry",
                Some(ImageSignature::ContainerPolicy),
            )));
            assert_eq!(ok, CheckResult::pass(names::IMAGE_SOURCE));
        }
        let bad = image_source(&Ok(booted(
            "quay.io/fedora/fedora-bootc:45",
            "oci",
            Some(ImageSignature::Insecure),
        )));
        assert_eq!(
            bad.outcome.to_string(),
            "fail: transport is oci, not registry; image quay.io/fedora/fedora-bootc:45 is \
             neither ghcr.io/apex-linux/apex:45 nor ghcr.io/apex-linux/apex-testing:45; \
             signature mode is insecure, not containerPolicy"
        );
        assert_eq!(
            image_source(&Ok(BootedImage::default()))
                .outcome
                .to_string(),
            "fail: bootc reports no booted container image"
        );
        assert_eq!(
            image_source(&Err("not root".to_owned()))
                .outcome
                .to_string(),
            "fail: bootc status failed: not root"
        );
    }

    #[test]
    fn numbers_follow_the_kernel() {
        assert_eq!(parse_number("0x1b"), Some(0x1b));
        assert_eq!(parse_number("0X1B"), Some(0x1b));
        assert_eq!(parse_number("27"), Some(27));
        assert_eq!(parse_number("033"), Some(0o33));
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number("0x"), None);
        assert_eq!(parse_number("08"), None);
        assert_eq!(parse_number("-1"), None);
        assert_eq!(
            parse_verb_line("0x20 0x500 0x1b trailing"),
            parse_verb_line("32 1280 27")
        );
        assert_eq!(parse_verb_line("0x20 0x500"), None);
    }

    #[test]
    fn patch_verbs_reads_only_verb_sections() {
        let verbs = patch_verbs(FIRMWARE).unwrap();
        assert_eq!(
            verbs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "0x20 0x500 0x001b",
                "0x20 0x400 0x4e4b",
                "0x20 0x500 0x000f",
                "0x20 0x400 0x7774"
            ]
        );
        assert_eq!(
            patch_verbs("[init_verbs]\n0x20 0x500 0x1b\n[hint]\njack_detect = yes\n").unwrap(),
            vec![HdaVerb {
                nid: 0x20,
                verb: 0x500,
                param: 0x1b
            }]
        );
        assert_eq!(
            patch_verbs("[verb]\n0x20 bogus\n").unwrap_err(),
            "line 2: not a verb: \"0x20 bogus\""
        );
    }

    #[test]
    fn constants_match_the_hardware_profile() {
        let text = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../hardware/m7400qc/profile.toml"
        ))
        .unwrap();
        let profile = toml::Value::Table(text.parse::<toml::Table>().unwrap());
        let get = |path: &str| {
            path.split('.')
                .try_fold(&profile, |v, key| v.get(key))
                .cloned()
                .unwrap_or_else(|| panic!("{path} missing from the profile"))
        };
        let int = |path: &str| u32::try_from(get(path).as_integer().unwrap()).unwrap();
        assert_eq!(get("battery.name").as_str(), Some(m7400qc::BATTERY));
        assert_eq!(
            int("battery.charge_end_threshold"),
            m7400qc::CHARGE_END_THRESHOLD
        );
        assert_eq!(get("gpu.discrete.pci").as_str(), Some(m7400qc::DGPU_PCI));
        assert_eq!(int("fingerprint.usb_vendor"), m7400qc::FINGERPRINT_VENDOR);
        assert_eq!(int("fingerprint.usb_product"), m7400qc::FINGERPRINT_PRODUCT);
        assert_eq!(int("audio.codec.vendor_id"), m7400qc::CODEC_VENDOR_ID);
        assert_eq!(int("audio.codec.subsystem_id"), m7400qc::CODEC_SUBSYSTEM_ID);
        assert!(m7400qc::PATCH_FIRMWARE.ends_with(&format!(
            "alc294-{}.fw",
            get("machine.id").as_str().unwrap()
        )));
    }

    fn arb_verb() -> impl Strategy<Value = HdaVerb> {
        (0u32..0x80, 0u32..0x1000, 0u32..0x1_0000).prop_map(|(nid, verb, param)| HdaVerb {
            nid,
            verb,
            param,
        })
    }

    fn spell(n: u32, style: u8) -> String {
        match style % 3 {
            0 => format!("{n:#x}"),
            1 => n.to_string(),
            _ if n == 0 => "0".to_owned(),
            _ => format!("0{n:o}"),
        }
    }

    proptest! {
        #[test]
        fn sysfs_output_round_trips(verbs in proptest::collection::vec(arb_verb(), 0..8)) {
            let text = verbs.iter().fold(String::new(), |mut text, v| {
                let _ = writeln!(text, "{v}");
                text
            });
            prop_assert_eq!(sysfs_verbs(&text), verbs);
        }

        #[test]
        fn firmware_in_any_spelling_parses(
            verbs in proptest::collection::vec((arb_verb(), any::<[u8; 3]>()), 0..8),
            comment in "[ -~]{0,20}",
        ) {
            let mut text = format!("[codec]\n0x10ec0294 0x10431ab2 0\n[verb]\n# {comment}\n\n");
            for (v, styles) in &verbs {
                let _ = writeln!(
                    text,
                    "  {}\t{} {}  ",
                    spell(v.nid, styles[0]),
                    spell(v.verb, styles[1]),
                    spell(v.param, styles[2])
                );
            }
            let expected: Vec<HdaVerb> = verbs.iter().map(|(v, _)| *v).collect();
            prop_assert_eq!(patch_verbs(&text).unwrap(), expected.clone());
            prop_assert!(missing_verbs(&expected, &expected).is_empty());
        }

        #[test]
        fn parsers_never_panic(text in "\\PC{0,200}") {
            let _ = patch_verbs(&text);
            let _ = sysfs_verbs(&text);
        }
    }
}
