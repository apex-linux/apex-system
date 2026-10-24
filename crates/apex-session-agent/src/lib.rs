pub mod agent;
pub mod css;
pub mod display;
pub mod files;
pub mod guard;
pub mod notifications;
pub mod palette;
pub mod paths;
pub mod refresh;
pub mod update1;
pub mod updates;

pub const BINARY_PATH: &str = "/usr/libexec/apex/apex-session-agent";

pub const SCHEMA_ID: &str = "io.github.apex_linux.Apex.Desktop";

pub const RADIUS_KEY: &str = "window-corner-radius";

pub const LAYER_KEY: &str = "layer-enabled";

pub const REFRESH_KEY: &str = "lower-refresh-rate-on-battery";

pub const INTERFACE_SCHEMA_ID: &str = "org.gnome.desktop.interface";

pub const ACCENT_KEY: &str = "accent-color";

pub const USER_UNIT: &str = include_str!("../data/apex-session-agent.service");
