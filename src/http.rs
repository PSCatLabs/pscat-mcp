//! HTTP transport: `/mcp` (Streamable HTTP, authenticated) and `/healthz`.

use std::sync::Arc;

use crate::config::HttpConfig;
use crate::render::RenderService;

/// Serve until SIGTERM/SIGINT, then shut down gracefully.
pub async fn serve(config: HttpConfig, service: Arc<RenderService>) -> anyhow::Result<()> {
    let _ = (config, service);
    todo!("http transport")
}
