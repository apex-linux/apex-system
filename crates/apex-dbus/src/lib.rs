//! Tokio clients build the [`zbus::Connection`] inside the runtime. `GLib` main-loop clients
//! enable the `async-io` feature: zbus then drives connections built outside tokio on zruntime.

mod deployment;
mod error;
mod polkit;
mod proxy;
mod values;

pub use deployment::Deployment;
pub use error::{ApexError, ErrorKind};
pub use polkit::{
    POLKIT_APPLY, POLKIT_CHECK, POLKIT_DISCARD, POLKIT_DOWNLOAD, POLKIT_ROLLBACK, PolkitAction,
    PolkitDefault,
};
pub use proxy::Update1Proxy;
pub use values::{
    BootVerdict, CHANGE_FIELDS, Change, ChangeKind, Channel, State, UnknownValueError, ValueError,
};

/// Clients name zbus types through this re-export, never through a second, possibly
/// different, zbus dependency.
pub use zbus;

pub const BUS_NAME: &str = "io.github.apex_linux.Apex.Update1";

pub const OBJECT_PATH: &str = "/io/github/apex_linux/Apex/Update1";

pub const INTERFACE: &str = "io.github.apex_linux.Apex.Update1";

pub const ERROR_PREFIX: &str = "io.github.apex_linux.Apex.Update1.Error";

/// Authoritative: the proxy and every server must match it.
pub const INTROSPECTION_XML: &str = include_str!("../data/io.github.apex_linux.Apex.Update1.xml");
