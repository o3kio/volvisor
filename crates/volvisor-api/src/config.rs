//! HTTP-surface configuration (P0).

/// Default request body limit: 1 MiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Configuration of the HTTP surface itself.
///
/// This is deliberately *not* the daemon configuration: paths, provider
/// selection and wiring belong to `volvisord`. Only the knobs the HTTP layer
/// itself consumes live here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    /// Maximum accepted request body size in bytes. Larger bodies are
    /// rejected before parsing.
    pub max_body_bytes: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}
