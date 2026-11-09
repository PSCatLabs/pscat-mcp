//! MCP tool definitions (rmcp). Thin: every tool delegates to
//! [`RenderService`](crate::render::RenderService).

use std::sync::Arc;

use crate::render::RenderService;

/// The MCP server handler shared by the stdio and HTTP transports.
#[derive(Clone)]
pub struct PscatMcp {
    service: Arc<RenderService>,
}

impl PscatMcp {
    /// A handler over `service`.
    pub fn new(service: Arc<RenderService>) -> Self {
        PscatMcp { service }
    }

    /// The underlying service.
    pub fn service(&self) -> &Arc<RenderService> {
        &self.service
    }
}

/// Serve MCP over stdin/stdout until the client disconnects.
pub async fn serve_stdio(service: Arc<RenderService>) -> anyhow::Result<()> {
    let _ = service;
    todo!("stdio transport")
}
