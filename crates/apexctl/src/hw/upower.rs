//! Best-effort read of the charge-threshold properties of `UPower` over the system bus.

use std::time::Duration;

use apex_dbus::zbus;

/// The battery charge limit as `UPower` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpowerFacts {
    /// `ChargeThresholdSupported`.
    pub supported: Option<bool>,
    /// `ChargeThresholdEnabled`.
    pub enabled: Option<bool>,
    /// `ChargeEndThreshold`.
    pub end_threshold: Option<u32>,
}

/// How long to wait for `UPower` before reporting "unknown".
pub const UPOWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Reads the properties of `battery` (e.g. `BAT0`). `None` when `UPower` does not answer in
/// time or the device does not exist; a property this `UPower` lacks is `None` inside.
pub async fn read(connection: &zbus::Connection, battery: &str) -> Option<UpowerFacts> {
    let path = format!("/org/freedesktop/UPower/devices/battery_{battery}");
    let query = async {
        let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(connection)
            .destination("org.freedesktop.UPower")
            .ok()?
            .path(path.as_str())
            .ok()?
            .interface("org.freedesktop.UPower.Device")
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        // A device that UPower does not know answers every Get with an error.
        let native: String = proxy.get_property("NativePath").await.ok()?;
        if native.is_empty() {
            return None;
        }
        Some(UpowerFacts {
            supported: proxy.get_property("ChargeThresholdSupported").await.ok(),
            enabled: proxy.get_property("ChargeThresholdEnabled").await.ok(),
            end_threshold: proxy.get_property("ChargeEndThreshold").await.ok(),
        })
    };
    tokio::time::timeout(UPOWER_TIMEOUT, query)
        .await
        .ok()
        .flatten()
}
