use std::time::Duration;

use apex_dbus::zbus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpowerFacts {
    pub supported: Option<bool>,
    pub enabled: Option<bool>,
    pub end_threshold: Option<u32>,
}

pub const UPOWER_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn read(connection: &zbus::Connection, battery: &str) -> Option<UpowerFacts> {
    let path = format!("/org/freedesktop/UPower/devices/battery_{battery}");
    let query = async {
        let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(connection)
            .destination("org.freedesktop.UPower")
            .path(path.as_str())
            .interface("org.freedesktop.UPower.Device")
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
