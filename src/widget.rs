//! The MCP Apps image widget (#162), opt-in with `PSCAT_MCP_CHATGPT_WIDGET`.
//!
//! ChatGPT shows pictures through an MCP App: a `ui://` HTML resource that a
//! tool's descriptor points to, rendered in a sandboxed frame with the tool
//! result. This module holds that one static page and what points to it:
//!
//! * [`resource`]: the `resources/list` entry of [`URI`];
//! * [`read`]: its `resources/read` contents, `text/html;profile=mcp-app`,
//!   with the Content Security Policy in `_meta.ui.csp` (and the older
//!   `openai/widgetCSP` alias);
//! * [`tool_meta`]: the `_meta` the tools in [`TOOLS`] carry in
//!   `tools/list`, `ui.resourceUri` and the `openai/outputTemplate` alias.
//!
//! Tool results are not changed. The page reads the result's
//! `structuredContent` (`download_url` per artifact, or the sheet's one) and
//! shows each picture whose link is `https://` and not expired, so it only
//! has pictures to show with `delivery` `url` or `both` on the `gcs` links.
//! The page is a constant: no request or result text is ever put into it.

use rmcp::model::{MetaObject, ReadResourceResult, Resource, ResourceContents};
use serde_json::{Value, json};

use crate::artifacts::LinksBackend;
use crate::config::ArtifactConfig;

/// The widget's resource URI. URIs are cache keys for the host, so a
/// breaking change to the page takes a new version here.
pub const URI: &str = "ui://pscat/render-result-v1";

/// The MIME type MCP Apps requires of a UI resource.
pub const MIME_TYPE: &str = "text/html;profile=mcp-app";

/// The page: static HTML and a small script, no external scripts, styles
/// or fonts.
pub const HTML: &str = include_str!("widget.html");

/// The origin V4 signed URLs name (`gcs` links), where a short link
/// redirects.
pub const SIGNED_URL_ORIGIN: &str = "https://storage.googleapis.com";

/// The tools whose descriptor points to the widget: those that return
/// pictures. The other job tools return none.
pub const TOOLS: [&str; 5] = [
    "render_postscript",
    "chalkboard_math",
    "handwrite",
    "review_sheet",
    "render_result",
];

/// The origins the page may load images from (`resourceDomains`): with
/// `gcs`, the short-link origin when one is set (the `<img>` request goes
/// there first, then follows the redirect, which the policy checks again)
/// and Cloud Storage; with memory or no links, none (those links are not
/// web links).
pub fn resource_domains(config: &ArtifactConfig) -> Vec<String> {
    match config.artifact_links {
        LinksBackend::Gcs => {
            let mut domains = Vec::new();
            if let Ok(Some(origin)) = config.public_origin() {
                domains.push(origin);
            }
            domains.push(SIGNED_URL_ORIGIN.to_string());
            domains
        }
        LinksBackend::Memory | LinksBackend::Off => Vec::new(),
    }
}

/// The `resources/list` entry.
pub fn resource() -> Resource {
    Resource::new(URI, "render-result")
        .with_title("PSCat render result")
        .with_description(
            "An MCP App view of a render: the pictures at the result's https links, each with \
             its link as text.",
        )
        .with_mime_type(MIME_TYPE)
        .with_size(HTML.len() as u64)
}

/// The `_meta` of the resource contents: the policy (camelCase, MCP Apps)
/// and its older ChatGPT alias (snake_case), and a border.
pub fn contents_meta(config: &ArtifactConfig) -> MetaObject {
    let domains = resource_domains(config);
    let Value::Object(map) = json!({
        "ui": {
            "csp": {
                "connectDomains": [],
                "resourceDomains": domains,
            },
            "prefersBorder": true,
        },
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": domains,
        },
    }) else {
        unreachable!("an object literal")
    };
    MetaObject(map)
}

/// `resources/read` of [`URI`].
pub fn read(config: &ArtifactConfig) -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(HTML, URI)
            .with_mime_type(MIME_TYPE)
            .with_meta(contents_meta(config)),
    ])
}

/// The `_meta` a tool in [`TOOLS`] carries: `ui.resourceUri`, and
/// `openai/outputTemplate`, the alias ChatGPT still honours.
pub fn tool_meta() -> MetaObject {
    let Value::Object(map) = json!({
        "ui": { "resourceUri": URI },
        "openai/outputTemplate": URI,
    }) else {
        unreachable!("an object literal")
    };
    MetaObject(map)
}

/// The startup warning for `config` with the widget on, if it has nothing
/// to show pictures from.
pub fn startup_warning(config: &ArtifactConfig) -> Option<&'static str> {
    if !config.chatgpt_widget {
        return None;
    }
    match config.artifact_links {
        LinksBackend::Gcs => None,
        LinksBackend::Off => Some(
            "PSCAT_MCP_CHATGPT_WIDGET is on but PSCAT_MCP_ARTIFACT_LINKS is off: results are \
             inline only, so the widget can show no pictures, only a note to ask for delivery url",
        ),
        LinksBackend::Memory => Some(
            "PSCAT_MCP_CHATGPT_WIDGET is on but PSCAT_MCP_ARTIFACT_LINKS is memory: its links are \
             pscat:// resources, not web links, so the widget can show no pictures; use gcs",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_follows_the_links() {
        let off = ArtifactConfig::default();
        assert!(resource_domains(&off).is_empty());
        assert!(resource_domains(&off.clone().memory()).is_empty());
        let gcs = ArtifactConfig::default().gcs("example-bucket");
        assert_eq!(resource_domains(&gcs), [SIGNED_URL_ORIGIN]);
        let direct = ArtifactConfig {
            artifact_public_origin: Some("direct".into()),
            ..gcs.clone()
        };
        assert_eq!(resource_domains(&direct), [SIGNED_URL_ORIGIN]);
        let short = ArtifactConfig {
            artifact_public_origin: Some("https://mcp.example.com".into()),
            ..gcs
        };
        assert_eq!(
            resource_domains(&short),
            ["https://mcp.example.com", SIGNED_URL_ORIGIN]
        );
        // A public origin means nothing without gcs.
        let memory = ArtifactConfig {
            artifact_public_origin: Some("https://mcp.example.com".into()),
            ..ArtifactConfig::default().memory()
        };
        assert!(resource_domains(&memory).is_empty());
    }

    #[test]
    fn the_contents_carry_both_policy_spellings() {
        let short = ArtifactConfig {
            artifact_public_origin: Some("https://mcp.example.com".into()),
            ..ArtifactConfig::default().gcs("example-bucket")
        };
        let meta = Value::Object(contents_meta(&short).0);
        let want = json!(["https://mcp.example.com", SIGNED_URL_ORIGIN]);
        assert_eq!(meta["ui"]["csp"]["resourceDomains"], want, "{meta}");
        assert_eq!(meta["ui"]["csp"]["connectDomains"], json!([]), "{meta}");
        assert_eq!(meta["ui"]["prefersBorder"], true, "{meta}");
        assert_eq!(meta["openai/widgetCSP"]["resource_domains"], want, "{meta}");
        assert_eq!(
            meta["openai/widgetCSP"]["connect_domains"],
            json!([]),
            "{meta}"
        );
        let memory = Value::Object(contents_meta(&ArtifactConfig::default().memory()).0);
        assert_eq!(memory["ui"]["csp"]["resourceDomains"], json!([]));
        assert_eq!(memory["openai/widgetCSP"]["resource_domains"], json!([]));
    }

    #[test]
    fn the_page_is_static_and_builds_text_nodes() {
        let lower = HTML.to_ascii_lowercase();
        assert!(lower.starts_with("<!doctype html>"));
        // No external script, style, font or frame.
        assert_eq!(lower.matches("<script").count(), 1);
        assert!(lower.contains("<script>"), "an inline script only");
        for banned in [
            "<script src",
            "src=\"http",
            "href=\"http",
            "<link",
            "@import",
            "url(",
            "<iframe",
            "eval(",
            "new function",
            "innerhtml",
            "outerhtml",
            "insertadjacenthtml",
            "document.write",
            "fetch(",
            "xmlhttprequest",
        ] {
            assert!(!lower.contains(banned), "{banned}");
        }
        // Text goes in with textContent; an image only from an https link.
        assert!(HTML.contains(".textContent = String(text)"));
        assert!(HTML.contains("u.indexOf(\"https://\") === 0"));
        assert!(HTML.contains("if (!isHttps(it.url))"));
        assert!(HTML.contains("img.src = it.url"));
        assert!(HTML.contains("prefers-color-scheme: dark"));
        // openai:set_globals also fires for theme and size changes: the page
        // redraws (and refetches the image) only for a new tool output.
        assert!(HTML.contains("if (out === lastOutput) { return; }"));
        assert!(HTML.contains("var json = JSON.stringify(out);"));
        assert!(HTML.contains("if (json === lastJson) { return; }"));
        let guard = HTML.find("if (json === lastJson)").unwrap();
        let draw = HTML[guard..].find("render(out)").map(|i| guard + i);
        assert!(draw.is_some(), "render(out) comes after the checks");
        // Both ways the result arrives, and the handshake.
        for needle in [
            "window.openai",
            "toolOutput",
            "ui/initialize",
            "ui/notifications/initialized",
            "ui/notifications/tool-result",
            "structuredContent",
            "delivery \\\"url\\\"",
        ] {
            assert!(HTML.contains(needle), "{needle}");
        }
    }

    #[test]
    fn the_descriptor_meta_has_both_keys() {
        let meta = Value::Object(tool_meta().0);
        assert_eq!(meta["ui"]["resourceUri"], URI);
        assert_eq!(meta["openai/outputTemplate"], URI);
    }

    #[test]
    fn warnings_name_what_is_missing() {
        let on = |c: ArtifactConfig| ArtifactConfig {
            chatgpt_widget: true,
            ..c
        };
        assert!(startup_warning(&ArtifactConfig::default()).is_none());
        assert!(
            startup_warning(&on(ArtifactConfig::default()))
                .unwrap()
                .contains("PSCAT_MCP_ARTIFACT_LINKS is off")
        );
        assert!(
            startup_warning(&on(ArtifactConfig::default().memory()))
                .unwrap()
                .contains("memory")
        );
        assert!(startup_warning(&on(ArtifactConfig::default().gcs("example-bucket"))).is_none());
    }
}
