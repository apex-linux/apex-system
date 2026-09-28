//! Tokio clients build the [`zbus::Connection`] inside the runtime. `GLib` main-loop clients
//! enable the `async-io` feature: zbus 5.19 then drives connections built outside tokio itself.
//!
//! ```no_run
//! # async fn example() -> Result<(), apex_dbus::ApexError> {
//! use apex_dbus::{ApexError, State, Update1Proxy};
//!
//! let connection = zbus::Connection::system().await?;
//! let proxy = Update1Proxy::new(&connection).await?;
//! if proxy.state().await? == State::Staged {
//!     let staged = proxy.staged().await?;
//!     match proxy.apply(staged.digest.as_deref().unwrap_or_default()).await {
//!         Ok(()) => println!("reboot to finish"),
//!         Err(ApexError::DigestMismatch(message)) => eprintln!("the stage changed: {message}"),
//!         Err(other) => return Err(other),
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod deployment;
mod error;
mod polkit;
mod proxy;
mod values;

pub use deployment::Deployment;
pub use error::{ApexError, ErrorKind};
pub use polkit::{
    POLKIT_APPLY, POLKIT_CHECK, POLKIT_DISCARD, POLKIT_DOWNGRADE, POLKIT_DOWNLOAD, POLKIT_ROLLBACK,
    POLKIT_SWITCH_CHANNEL, PolkitAction, PolkitDefault,
};
pub use proxy::Update1Proxy;
pub use values::{
    BootVerdict, CHANGE_FIELDS, Change, ChangeKind, Channel, State, UnknownValueError, ValueError,
};

/// Clients name zbus types through this re-export, never through a second, possibly
/// different, zbus dependency.
pub use zbus;

pub const BUS_NAME: &str = "io.github.sekirokenjii.Apex.Update1";

pub const OBJECT_PATH: &str = "/io/github/sekirokenjii/Apex/Update1";

pub const INTERFACE: &str = "io.github.sekirokenjii.Apex.Update1";

pub const ERROR_PREFIX: &str = "io.github.sekirokenjii.Apex.Update1.Error";

/// Authoritative: the proxy and every server must match it.
pub const INTROSPECTION_XML: &str = include_str!("../data/io.github.sekirokenjii.Apex.Update1.xml");
