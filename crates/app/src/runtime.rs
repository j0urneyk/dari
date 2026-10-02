//! The Tokio runtime that networking and media run on, shared with the GPUI app.

use std::future::Future;

use gpui_kit::{App, Global};

/// Owns the Tokio runtime for the app's lifetime. Network sessions spawn their tasks on it;
/// GPUI tasks await their results, since Tokio's handles and channels work on any executor.
pub struct TokioRuntime(tokio::runtime::Runtime);

impl Global for TokioRuntime {}

impl std::fmt::Debug for TokioRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioRuntime").finish_non_exhaustive()
    }
}

impl TokioRuntime {
    pub fn install(runtime: tokio::runtime::Runtime, cx: &mut App) {
        cx.set_global(Self(runtime));
    }

    /// Spawns `future` on Tokio and returns its join handle, awaitable from GPUI tasks.
    pub fn spawn<F>(cx: &App, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        cx.global::<Self>().0.spawn(future)
    }

    /// Runs `function` with the runtime entered, for APIs that spawn tasks synchronously.
    pub fn enter<R>(cx: &App, function: impl FnOnce() -> R) -> R {
        let _entered = cx.global::<Self>().0.enter();
        function()
    }
}
