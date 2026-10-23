use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub mod auth;
pub mod channel;
pub mod daemon;
pub mod engine;
pub mod errors;
pub mod properties;
pub mod providers;
pub mod queue;
pub mod service;
pub mod store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod text;
pub mod view;

/// Keeps the provider and authorizer traits dyn-compatible.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub const BINARY_PATH: &str = "/usr/libexec/apex/apex-updated";

pub const REFRESH_MAX_AGE: Duration = Duration::from_secs(10);
