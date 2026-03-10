/// Errors that can occur when spawning zen overlays.
///
/// Returned by [`ZenWindowBuilder::spawn`](crate::ZenWindowBuilder::spawn).
/// Callers can match on variants to distinguish failure modes and decide
/// whether to retry or fall back gracefully.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// No Wayland display could be reached.
    ///
    /// Usually means `$WAYLAND_DISPLAY` is unset or the compositor isn't
    /// running.
    #[error("failed to connect to Wayland display")]
    WaylandConnection(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A required Wayland protocol is not advertised by the compositor.
    ///
    /// `protocol` names the missing global (e.g. `"wl_compositor"`,
    /// `"zwlr_layer_shell_v1"`, `"wl_shm"`).
    #[error("required Wayland protocol unavailable: {protocol}")]
    MissingProtocol {
        /// The Wayland global interface name that was not found
        /// (e.g. `"wl_compositor"`, `"zwlr_layer_shell_v1"`).
        protocol: &'static str,
        /// The underlying error from the registry bind attempt.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The OS refused to create a background thread.
    #[error("failed to spawn background thread")]
    ThreadSpawn(#[source] std::io::Error),

    /// Wayland setup failed after connecting but before overlays were ready.
    ///
    /// Covers registry initialization, shared-memory pool creation, and
    /// initial roundtrip failures.
    #[error("Wayland setup failed")]
    Setup(#[source] Box<dyn std::error::Error + Send + Sync>),
}
