//! Version information baked in at build time.

use serde::Serialize;

/// This server's crate version.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The PSCat crate version from `Cargo.lock`.
pub const PSCAT_VERSION: &str = env!("PSCAT_VERSION");
/// The PSCat source from `Cargo.lock` (git URL with the pinned commit).
pub const PSCAT_SOURCE: &str = env!("PSCAT_SOURCE");

/// The pinned PSCat git commit, if the dependency is a git source.
pub fn pscat_revision() -> Option<&'static str> {
    PSCAT_SOURCE.rsplit_once('#').map(|(_, rev)| rev)
}

/// Renderer identity reported in tool results and `get_renderer_info`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RendererIdentity {
    /// Always `pscat`.
    pub engine: &'static str,
    /// PSCat crate version.
    pub version: &'static str,
    /// Pinned PSCat git commit, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<&'static str>,
}

impl RendererIdentity {
    /// The identity of the renderer compiled into this binary.
    pub fn current() -> Self {
        RendererIdentity {
            engine: "pscat",
            version: PSCAT_VERSION,
            revision: pscat_revision(),
        }
    }
}
