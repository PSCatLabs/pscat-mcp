//! The opt-in MCP Apps image widget (#162): with `PSCAT_MCP_CHATGPT_WIDGET`
//! off, `tools/list` and `resources/list` are what they were; on, the
//! `ui://` resource is listed and read, and the picture tools point to it.
//! Tool results are the same either way (`tests/artifacts.rs`).

use std::sync::Arc;

use pscat_mcp::config::{ArtifactConfig, JobsConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::mcp::PscatMcp;
use pscat_mcp::render::RenderService;
use pscat_mcp::widget;
use rmcp::model::{
    ClientCapabilities, ClientConfig, Implementation, ReadResourceRequestParams, ResourceContents,
};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ClientConfig>;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");

fn service(artifacts: ArtifactConfig) -> Arc<RenderService> {
    let worker = WorkerConfig {
        worker_exe: Some(SERVER.into()),
        ..WorkerConfig::default()
    };
    pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig::default().enabled(),
        artifacts,
        &worker,
    )
    .expect("service")
}

fn on(c: ArtifactConfig) -> ArtifactConfig {
    ArtifactConfig {
        chatgpt_widget: true,
        ..c
    }
}

async fn connect(service: Arc<RenderService>) -> Client {
    let (server_io, client_io) = tokio::io::duplex(1 << 22);
    let server = PscatMcp::new(service);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::from_build_env(),
    )
    .serve(client_io)
    .await
    .expect("client initialize")
}

async fn tools(client: &Client) -> Value {
    serde_json::to_value(client.list_all_tools().await.expect("tools/list")).unwrap()
}

async fn resources(client: &Client) -> Value {
    serde_json::to_value(client.list_all_resources().await.expect("resources/list")).unwrap()
}

fn strip_meta(mut tools: Value) -> Value {
    for t in tools.as_array_mut().unwrap() {
        t.as_object_mut().unwrap().remove("_meta");
    }
    tools
}

#[tokio::test(flavor = "multi_thread")]
async fn off_nothing_changes_and_on_only_meta_and_one_resource_are_added() {
    for links in [
        ArtifactConfig::default(),
        ArtifactConfig::default().memory(),
    ] {
        let off = connect(service(links.clone())).await;
        let on = connect(service(on(links))).await;
        let (off_tools, on_tools) = (tools(&off).await, tools(&on).await);
        let off_text = off_tools.to_string();
        assert!(!off_text.contains("\"_meta\""), "{off_text}");
        assert!(!off_text.contains("ui://"), "{off_text}");
        assert!(!off_text.contains("openai/"), "{off_text}");
        // On: the same bytes once `_meta` is taken out (descriptions,
        // schemas and order alike).
        assert_eq!(
            strip_meta(on_tools.clone()).to_string(),
            off_text,
            "only _meta differs"
        );
        for t in on_tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            if widget::TOOLS.contains(&name) {
                assert_eq!(
                    t["_meta"],
                    json!({"ui": {"resourceUri": widget::URI}, "openai/outputTemplate": widget::URI}),
                    "{name}"
                );
            } else {
                assert!(t.get("_meta").is_none(), "{name}: {t}");
            }
        }
        // Every widget tool is listed here (jobs are on).
        for name in widget::TOOLS {
            assert!(
                on_tools
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name),
                "{name}"
            );
        }
        let (off_res, mut on_res) = (resources(&off).await, resources(&on).await);
        assert!(!off_res.to_string().contains("ui://"), "{off_res}");
        let last = on_res.as_array_mut().unwrap().pop().unwrap();
        assert_eq!(on_res, off_res, "the guide resources come first, unchanged");
        assert_eq!(last["uri"], widget::URI);
        assert_eq!(last["mimeType"], widget::MIME_TYPE);
        assert!(
            off_res
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["uri"].as_str().unwrap().starts_with("pscat://guide/")),
            "{off_res}"
        );
        // Off, the URI is not a resource: the same error as any unknown one.
        let err = off
            .read_resource(ReadResourceRequestParams::new(widget::URI))
            .await
            .unwrap_err()
            .to_string();
        let unknown = off
            .read_resource(ReadResourceRequestParams::new("ui://pscat/nothing"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err.replace(widget::URI, "URI"),
            unknown.replace("ui://pscat/nothing", "URI")
        );
        off.cancel().await.ok();
        on.cancel().await.ok();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_the_page_is_read_with_its_policy_and_the_guide_still_is() {
    let client = connect(service(on(ArtifactConfig::default().memory()))).await;
    let r = client
        .read_resource(ReadResourceRequestParams::new(widget::URI))
        .await
        .expect("resources/read");
    let [
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            meta,
        },
    ] = r.contents.as_slice()
    else {
        panic!("one text: {r:?}");
    };
    assert_eq!(uri, widget::URI);
    assert_eq!(mime_type.as_deref(), Some("text/html;profile=mcp-app"));
    assert_eq!(text, widget::HTML);
    let meta = serde_json::to_value(meta).unwrap();
    assert_eq!(meta["ui"]["csp"]["resourceDomains"], json!([]), "{meta}");
    assert_eq!(meta["ui"]["prefersBorder"], true, "{meta}");
    assert_eq!(meta["openai/widgetCSP"]["resource_domains"], json!([]));
    // The guide documents are still listed and read.
    let listed = resources(&client).await;
    let guide = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["uri"].as_str().unwrap().starts_with("pscat://guide/"))
        .expect("a guide resource")["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let doc = client
        .read_resource(ReadResourceRequestParams::new(&guide))
        .await
        .expect("guide read");
    assert!(
        matches!(&doc.contents[0], ResourceContents::TextResourceContents { mime_type, .. }
            if mime_type.as_deref() == Some("text/markdown")),
        "{doc:?}"
    );
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_jobs_render_result_is_absent_and_the_rest_still_point_to_the_page() {
    let worker = WorkerConfig {
        worker_exe: Some(SERVER.into()),
        ..WorkerConfig::default()
    };
    let service = pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig::default(),
        on(ArtifactConfig::default()),
        &worker,
    )
    .expect("service");
    let client = connect(service).await;
    let listed = tools(&client).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t.get("_meta").is_some())
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names.len(),
        4,
        "{names:?}: the four picture tools, no render_result"
    );
    assert!(!names.contains(&"render_result"));
    client.cancel().await.ok();
}

/// Run `serve-stdio` with `args` and `env` and no client; return its exit
/// status and stderr. A server that starts exits at once on empty stdin.
fn run_server(args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = std::process::Command::new(SERVER);
    cmd.arg("serve-stdio")
        .args(args)
        .env("PSCAT_MCP_LOG_FORMAT", "pretty")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the server");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_widget_starts_and_warns_when_it_has_no_web_links() {
    let (_, err) = run_server(&[], &[("PSCAT_MCP_CHATGPT_WIDGET", "true")]);
    assert!(err.contains("PSCAT_MCP_ARTIFACT_LINKS is off"), "{err}");
    assert!(!err.contains("refusing"), "{err}");
    let (_, err) = run_server(
        &["--chatgpt-widget"],
        &[("PSCAT_MCP_ARTIFACT_LINKS", "memory")],
    );
    assert!(err.contains("PSCAT_MCP_ARTIFACT_LINKS is memory"), "{err}");
    let (_, err) = run_server(&[], &[]);
    assert!(!err.contains("PSCAT_MCP_CHATGPT_WIDGET"), "{err}");
}
