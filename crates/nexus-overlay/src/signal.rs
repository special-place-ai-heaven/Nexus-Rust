/// Non-blocking runtime shutdown signal invoked from the selected window thread.
pub trait ShutdownSignal: Send + Sync + 'static {
    /// Requests orderly runtime shutdown after the primary window is destroyed.
    fn request_shutdown(&self);
}

impl<F> ShutdownSignal for F
where
    F: Fn() + Send + Sync + 'static,
{
    fn request_shutdown(&self) {
        self();
    }
}

/// Shutdown signal used when runtime lifecycle integration is not installed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopShutdownSignal;

impl ShutdownSignal for NoopShutdownSignal {
    fn request_shutdown(&self) {}
}
