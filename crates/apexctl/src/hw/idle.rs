//! ACCEPTANCE 4.2 and 4.9's idle window, and ROADMAP N10's wakes per idle hour, from
//! cached sysfs attributes only: nothing here wakes the dGPU.

use std::fmt::Write as _;

use serde::Serialize;

use super::SysRoot;
use super::machine::{BATTERY, DGPU_PCI};

pub const DEFAULT_SECONDS: u64 = 600;

/// ACCEPTANCE 4.2: a suspended share of at least 95.0 %.
pub const PASS_PERMILLE: u64 = 950;

const BATTERY_EVERY_S: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdleReport {
    pub window_s: u64,
    pub control: Option<String>,
    pub driver: Option<String>,
    pub active_ms: u64,
    pub suspended_ms: u64,
    pub suspended_permille: u64,
    /// Seen at 1 s sampling: a shorter wake can fall between two samples, though the
    /// active time still counts it.
    pub wakes: u64,
    pub wakes_per_hour: u64,
    pub battery_status: Option<String>,
    /// Meaningful on battery only.
    pub battery_mw: Option<u64>,
    pub runtime_status: Option<String>,
    pub power_state: Option<String>,
    pub pass: bool,
}

struct Paths {
    device: String,
    battery: String,
}

impl Paths {
    fn new() -> Self {
        Self {
            device: format!("/sys/bus/pci/devices/{DGPU_PCI}"),
            battery: format!("/sys/class/power_supply/{BATTERY}"),
        }
    }

    fn device(&self, attribute: &str) -> String {
        format!("{}/{attribute}", self.device)
    }

    fn battery(&self, attribute: &str) -> String {
        format!("{}/{attribute}", self.battery)
    }
}

fn number(root: &SysRoot, path: &str) -> Option<u64> {
    root.read(path).ok().flatten()?.parse().ok()
}

fn text(root: &SysRoot, path: &str) -> Option<String> {
    root.read(path).ok().flatten()
}

fn runtime_times(root: &SysRoot, paths: &Paths) -> Result<(u64, u64), String> {
    let time = |name: &str| {
        let path = paths.device(&format!("power/{name}"));
        number(root, &path).ok_or_else(|| format!("cannot read {path}"))
    };
    Ok((
        time("runtime_active_time")?,
        time("runtime_suspended_time")?,
    ))
}

/// In µW: `power_now`, or `current_now` times `voltage_now`.
fn battery_uw(root: &SysRoot, paths: &Paths) -> Option<u64> {
    number(root, &paths.battery("power_now")).or_else(|| {
        let current = number(root, &paths.battery("current_now"))?;
        let voltage = number(root, &paths.battery("voltage_now"))?;
        Some(current.saturating_mul(voltage) / 1_000_000)
    })
}

/// `wait(t)` returns after second `t` of the window; on the machine it sleeps one second.
pub fn measure(
    root: &SysRoot,
    seconds: u64,
    mut wait: impl FnMut(u64),
) -> Result<IdleReport, String> {
    let paths = Paths::new();
    if root.read(&paths.device("vendor")).ok().flatten().is_none() {
        return Err(format!("no PCI device at {DGPU_PCI}"));
    }
    let control = text(root, &paths.device("power/control"));
    let driver = std::fs::read_link(root.path(&paths.device("driver")))
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
    let (active_before, suspended_before) = runtime_times(root, &paths)?;
    let status = || text(root, &paths.device("power/runtime_status"));
    let mut previous = status();
    let (mut wakes, mut uw_sum, mut uw_samples) = (0_u64, 0_u64, 0_u64);
    for t in 1..=seconds {
        wait(t);
        let current = status();
        if previous.as_deref() == Some("suspended") && current.as_deref() != Some("suspended") {
            wakes += 1;
        }
        previous = current;
        if t % BATTERY_EVERY_S == 0
            && let Some(uw) = battery_uw(root, &paths)
        {
            uw_sum = uw_sum.saturating_add(uw);
            uw_samples += 1;
        }
    }
    let (active_after, suspended_after) = runtime_times(root, &paths)?;
    let active_ms = active_after.saturating_sub(active_before);
    let suspended_ms = suspended_after.saturating_sub(suspended_before);
    let total = active_ms + suspended_ms;
    if total == 0 {
        return Err("no runtime PM accounting (is power/control \"auto\"?)".to_owned());
    }
    let suspended_permille = suspended_ms * 1000 / total;
    Ok(IdleReport {
        window_s: seconds,
        control,
        driver,
        active_ms,
        suspended_ms,
        suspended_permille,
        wakes,
        wakes_per_hour: wakes * 3600 / seconds.max(1),
        battery_status: text(root, &paths.battery("status")),
        battery_mw: (uw_samples > 0).then(|| uw_sum / uw_samples / 1000),
        runtime_status: status(),
        power_state: text(root, &paths.device("power_state")),
        pass: suspended_permille >= PASS_PERMILLE,
    })
}

pub const DEFAULT_D3COLD_WITHIN_S: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct D3coldWait {
    pub within_s: u64,
    /// `None` when the window ran out first.
    pub d3cold_after_s: Option<u64>,
    pub power_state: Option<String>,
}

/// ACCEPTANCE §4's `d3cold_within`: polls `power_state` once a second, which never wakes
/// the dGPU.
pub fn wait_for_d3cold(
    root: &SysRoot,
    within_s: u64,
    mut wait: impl FnMut(u64),
) -> Result<D3coldWait, String> {
    let paths = Paths::new();
    if root.read(&paths.device("vendor")).ok().flatten().is_none() {
        return Err(format!("no PCI device at {DGPU_PCI}"));
    }
    let mut power_state = None;
    for s in 0..=within_s {
        power_state = text(root, &paths.device("power_state"));
        if power_state.as_deref() == Some("D3cold") {
            return Ok(D3coldWait {
                within_s,
                d3cold_after_s: Some(s),
                power_state,
            });
        }
        if s < within_s {
            wait(s + 1);
        }
    }
    Ok(D3coldWait {
        within_s,
        d3cold_after_s: None,
        power_state,
    })
}

pub fn render_d3cold(result: &D3coldWait) -> String {
    match result.d3cold_after_s {
        Some(s) => format!("D3cold after {s} s\n"),
        None => format!(
            "FAIL: {} after {} s\n",
            result.power_state.as_deref().unwrap_or("unknown"),
            result.within_s
        ),
    }
}

pub fn render(report: &IdleReport) -> String {
    let unknown = |value: &Option<String>| value.clone().unwrap_or_else(|| "unknown".to_owned());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "control={} driver={}",
        unknown(&report.control),
        report.driver.as_deref().unwrap_or("none")
    );
    let _ = writeln!(
        out,
        "window {} s: active {} ms, suspended {} ms, suspended share {}.{} %",
        report.window_s,
        report.active_ms,
        report.suspended_ms,
        report.suspended_permille / 10,
        report.suspended_permille % 10
    );
    let _ = writeln!(
        out,
        "wakes seen (1 s sampling): {}, {} per hour",
        report.wakes, report.wakes_per_hour
    );
    if let Some(mw) = report.battery_mw {
        let _ = writeln!(
            out,
            "battery ({}): {mw} mW mean",
            unknown(&report.battery_status)
        );
    }
    let _ = writeln!(
        out,
        "now: runtime_status={} power_state={}",
        unknown(&report.runtime_status),
        unknown(&report.power_state)
    );
    out.push_str(if report.pass {
        "PASS\n"
    } else {
        "FAIL: below 95.0 %\n"
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDgpu {
        dir: tempfile::TempDir,
    }

    impl FakeDgpu {
        fn new() -> Self {
            let fake = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            fake.device("vendor", "0x10de")
                .device("power/control", "auto")
                .device("power/runtime_status", "suspended")
                .device("power_state", "D3cold")
                .times(1_000, 50_000);
            let driver = fake.dir.path().join("sys/bus/pci/drivers/nvidia");
            std::fs::create_dir_all(&driver).unwrap();
            std::os::unix::fs::symlink(
                driver,
                fake.dir
                    .path()
                    .join(format!("sys/bus/pci/devices/{DGPU_PCI}/driver")),
            )
            .unwrap();
            fake
        }

        fn write(&self, absolute: &str, value: &str) -> &Self {
            let path = self.dir.path().join(absolute.trim_start_matches('/'));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{value}\n")).unwrap();
            self
        }

        fn device(&self, attribute: &str, value: &str) -> &Self {
            self.write(&Paths::new().device(attribute), value)
        }

        fn battery(&self, attribute: &str, value: &str) -> &Self {
            self.write(&Paths::new().battery(attribute), value)
        }

        fn times(&self, active: u64, suspended: u64) -> &Self {
            self.device("power/runtime_active_time", &active.to_string())
                .device("power/runtime_suspended_time", &suspended.to_string())
        }

        fn root(&self) -> SysRoot {
            SysRoot::new(self.dir.path())
        }
    }

    #[test]
    fn an_idle_window_with_one_wake_passes() {
        let fake = FakeDgpu::new();
        fake.battery("status", "Discharging")
            .battery("power_now", "4200000");
        let report = measure(&fake.root(), 20, |t| match t {
            5 => {
                fake.device("power/runtime_status", "active");
            }
            6 => {
                fake.device("power/runtime_status", "suspended");
            }
            20 => {
                fake.times(1_500, 69_500);
            }
            _ => {}
        })
        .unwrap();
        assert_eq!(
            report,
            IdleReport {
                window_s: 20,
                control: Some("auto".to_owned()),
                driver: Some("nvidia".to_owned()),
                active_ms: 500,
                suspended_ms: 19_500,
                suspended_permille: 975,
                wakes: 1,
                wakes_per_hour: 180,
                battery_status: Some("Discharging".to_owned()),
                battery_mw: Some(4_200),
                runtime_status: Some("suspended".to_owned()),
                power_state: Some("D3cold".to_owned()),
                pass: true,
            }
        );
        insta::assert_snapshot!(render(&report));
    }

    #[test]
    fn a_busy_window_fails_and_battery_power_falls_back_to_current_times_voltage() {
        let fake = FakeDgpu::new();
        fake.battery("status", "Discharging")
            .battery("current_now", "500000")
            .battery("voltage_now", "16000000");
        let report = measure(&fake.root(), 10, |t| {
            if t == 10 {
                fake.times(3_000, 58_000);
            }
        })
        .unwrap();
        assert_eq!(report.suspended_permille, 800);
        assert!(!report.pass);
        assert_eq!(report.battery_mw, Some(8_000));
        assert!(render(&report).ends_with("FAIL: below 95.0 %\n"));
    }

    #[test]
    fn no_device_or_no_accounting_is_an_error() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            measure(&SysRoot::new(empty.path()), 5, |_| {}),
            Err(format!("no PCI device at {DGPU_PCI}"))
        );
        let fake = FakeDgpu::new();
        let error = measure(&fake.root(), 5, |_| {}).unwrap_err();
        assert!(error.starts_with("no runtime PM accounting"), "{error}");
    }

    #[test]
    fn d3cold_is_awaited_by_polling_power_state() {
        let fake = FakeDgpu::new();
        fake.device("power_state", "D0");
        let result = wait_for_d3cold(&fake.root(), 30, |s| {
            if s == 4 {
                fake.device("power_state", "D3cold");
            }
        })
        .unwrap();
        assert_eq!(result.d3cold_after_s, Some(4));
        assert_eq!(render_d3cold(&result), "D3cold after 4 s\n");

        let at_once = wait_for_d3cold(&fake.root(), 30, |_| panic!("no wait needed")).unwrap();
        assert_eq!(at_once.d3cold_after_s, Some(0));

        fake.device("power_state", "D0");
        let mut waits = 0;
        let timed_out = wait_for_d3cold(&fake.root(), 3, |_| waits += 1).unwrap();
        assert_eq!(waits, 3);
        assert_eq!(timed_out.d3cold_after_s, None);
        assert_eq!(render_d3cold(&timed_out), "FAIL: D0 after 3 s\n");
        let empty = tempfile::tempdir().unwrap();
        assert!(wait_for_d3cold(&SysRoot::new(empty.path()), 3, |_| {}).is_err());
    }

    #[test]
    fn an_unbound_dgpu_reads_driver_none_and_no_battery_line() {
        let fake = FakeDgpu::new();
        std::fs::remove_file(
            fake.dir
                .path()
                .join(format!("sys/bus/pci/devices/{DGPU_PCI}/driver")),
        )
        .unwrap();
        let report = measure(&fake.root(), 3, |t| {
            if t == 3 {
                fake.times(1_000, 53_000);
            }
        })
        .unwrap();
        assert_eq!(report.driver, None);
        assert_eq!(report.battery_mw, None, "no sample within 3 s");
        let text = render(&report);
        assert!(text.starts_with("control=auto driver=none\n"), "{text}");
        assert!(!text.contains("battery"), "{text}");
    }
}
