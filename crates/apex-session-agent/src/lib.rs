pub mod agent;
pub mod css;
pub mod files;
pub mod guard;
pub mod notifications;
pub mod paths;

pub const BINARY_PATH: &str = "/usr/libexec/apex/apex-session-agent";

pub const SCHEMA_ID: &str = "io.github.apex_linux.Apex.Desktop";

pub const RADIUS_KEY: &str = "window-corner-radius";

pub const LAYER_KEY: &str = "layer-enabled";

pub const USER_UNIT: &str = include_str!("../data/apex-session-agent.service");
