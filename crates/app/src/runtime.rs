//! Background work for the UI.
//!
//! sqlx needs Tokio; GPUI has its own executor. All database, file and keychain I/O
//! runs here, and GPUI tasks await the results.

use std::future::Future;
#[cfg(not(test))]
use std::sync::LazyLock;

#[cfg(not(test))]
static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().expect("failed to start the Tokio runtime"));

#[cfg(not(test))]
pub fn io<T: Send + 'static>(
    fut: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn(fut);
    async move { handle.await? }
}

#[cfg(not(test))]
pub fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn_blocking(f);
    async move { handle.await? }
}

// UI tests stop at the "busy" state and never run I/O: GPUI's test scheduler panics
// when a Tokio thread wakes one of its tasks.
#[cfg(test)]
pub fn io<T>(
    fut: impl Future<Output = anyhow::Result<T>>,
) -> impl Future<Output = anyhow::Result<T>> {
    drop(fut);
    std::future::pending()
}

#[cfg(test)]
pub fn blocking<T>(
    f: impl FnOnce() -> anyhow::Result<T>,
) -> impl Future<Output = anyhow::Result<T>> {
    drop(f);
    std::future::pending()
}
