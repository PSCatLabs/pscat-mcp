//! Artifact delivery (#139): returning rendered output as links instead of,
//! or as well as, inline content blocks.
//!
//! * [`Delivery`] is the `delivery` argument of `render_postscript`,
//!   `chalkboard_math`, `handwrite` and `review_sheet` (and the operator
//!   default, `PSCAT_MCP_ARTIFACT_DELIVERY`). `inline` is today's result,
//!   byte for byte.
//! * [`ArtifactStore`] keeps the finished bytes of one call under one random
//!   `render_id` and hands back one link per artifact, with an expiry. The
//!   server process only ever stores bytes a worker already produced; it
//!   never parses or runs them.
//! * [`MemoryArtifactStore`] is the in-memory backend
//!   (`PSCAT_MCP_ARTIFACT_LINKS=memory`). Its bytes are charged against the
//!   async-job results budget ([`crate::jobs::JobStore`] keeps the ledger),
//!   with a per-principal quota and a TTL, and are served by `resources/read`
//!   of `pscat://render/<render_id>/<n>`. It lives in one process: lost on a
//!   restart, and invisible to any other instance.
//! * [`crate::gcs::GcsArtifactStore`] is the Cloud Storage backend
//!   (`PSCAT_MCP_ARTIFACT_LINKS=gcs`, #140): `put` uploads, each link is an
//!   `https` V4 signed URL ([`LinkScheme::Https`]) with an expiry, and
//!   `read` returns nothing because it serves no bytes itself.
//!
//! Links are capabilities for whoever holds them (a signed URL is one
//! outright), so nothing here logs a link, a `render_id` or the bytes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::jobs::JobStore;
use crate::render::{ErrorCode, InputDiagnostic, RenderError};
use crate::short_link::LinkError;

/// How a tool returns what it rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Delivery {
    /// The bytes in the result (image, text or blob blocks), as before #139.
    #[default]
    Inline,
    /// Links only: no bytes in the result.
    Url,
    /// The bytes inline and the links.
    Both,
    /// Links when the output is over the threshold
    /// (`PSCAT_MCP_ARTIFACT_THRESHOLD_BYTES`), otherwise inline.
    Auto,
}

impl Delivery {
    /// Every value, as the `delivery` argument names them.
    pub const NAMES: [&'static str; 4] = ["inline", "url", "both", "auto"];

    /// The value named `name`.
    pub fn parse(name: &str) -> Option<Delivery> {
        match name {
            "inline" => Some(Delivery::Inline),
            "url" => Some(Delivery::Url),
            "both" => Some(Delivery::Both),
            "auto" => Some(Delivery::Auto),
            _ => None,
        }
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Inline => "inline",
            Delivery::Url => "url",
            Delivery::Both => "both",
            Delivery::Auto => "auto",
        }
    }

    /// What `self` comes to for output of `bytes` bytes, with `threshold`
    /// the `auto` threshold: `inline`, `url` or `both`, never `auto`.
    pub fn decide(self, bytes: usize, threshold: usize) -> Delivery {
        match self {
            Delivery::Auto if bytes > threshold => Delivery::Url,
            Delivery::Auto => Delivery::Inline,
            d => d,
        }
    }

    /// Whether the result carries the bytes inline.
    pub fn inline_bytes(self) -> bool {
        matches!(self, Delivery::Inline | Delivery::Both)
    }
}

/// Where links point (`PSCAT_MCP_ARTIFACT_LINKS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum LinksBackend {
    /// No artifact store: `delivery` `url` and `both` are refused.
    #[default]
    Off,
    /// In this process's memory, served by `resources/read`.
    Memory,
    /// Uploaded to Cloud Storage and linked by V4 signed URLs
    /// (`crate::gcs`).
    Gcs,
}

/// The URI prefix of a stored artifact (memory backend).
pub const RENDER_URI_PREFIX: &str = "pscat://render/";

/// What a stored render is charged against the budget besides its bytes:
/// its ids and bookkeeping.
pub const RENDER_CHARGE_BYTES: usize = 1024;

/// The URI of artifact `n` (from 1) of render `id`.
pub fn render_uri(id: &str, n: usize) -> String {
    format!("{RENDER_URI_PREFIX}{id}/{n}")
}

/// The render id and artifact number (from 1) of a render URI, if `uri` is
/// one.
pub fn parse_render_uri(uri: &str) -> Option<(&str, usize)> {
    let rest = uri.strip_prefix(RENDER_URI_PREFIX)?;
    let (id, n) = rest.split_once('/')?;
    let n: usize = n.parse().ok()?;
    (n >= 1 && !id.is_empty()).then_some((id, n))
}

/// `text` with the id of every render URI replaced by `[redacted]`, also
/// where JSON escapes the slashes (`pscat:\/\/render\/<id>`). Used where
/// request bodies are recorded (capture mode), so a link never lands in a
/// file.
pub fn redact_render_links(text: &str) -> String {
    const PREFIXES: [&str; 2] = [RENDER_URI_PREFIX, "pscat:\\/\\/render\\/"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = PREFIXES
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, p.len())))
            .min();
        let Some((i, len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i + len]);
        rest = &rest[i + len..];
        let id_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(rest.len());
        if id_len > 0 {
            out.push_str("[redacted]");
        }
        rest = &rest[id_len..];
    }
}

/// `path` (a URL without its query) with the render id of a short link
/// (`/a/<render_id>/...`, #163) replaced by `[redacted]`, also where JSON
/// escapes the slashes (`\/a\/<render_id>`).
fn redact_short_link_id(path: &str) -> String {
    const MARKS: [&str; 2] = ["/a/", "\\/a\\/"];
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    loop {
        let next = MARKS
            .iter()
            .filter_map(|m| rest.find(m).map(|i| (i, m.len())))
            .min();
        let Some((i, len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i + len]);
        rest = &rest[i + len..];
        let id_len = rest
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .unwrap_or(rest.len());
        if id_len > 0 {
            out.push_str("[redacted]");
        }
        rest = &rest[id_len..];
    }
}

/// `text` with the query string of every `http(s)://` URL replaced by
/// `[redacted]`, also where JSON escapes the slashes (`https:\/\/`). A signed
/// URL's query (`X-Goog-Signature`, ...) is what makes it a capability, so
/// a client that sends one back (in a tool argument, say) never has it
/// recorded (capture mode). A short link's render id, in its path, is
/// redacted too. A URL ends at whitespace, a quote, `<`, `>`, a
/// backtick, or a backslash escaping a quote or whitespace (`\"`, `\n`);
/// other JSON escapes (`\/`, `&`) are part of it.
pub fn redact_url_queries(text: &str) -> String {
    const SCHEMES: [&str; 4] = ["https://", "http://", "https:\\/\\/", "http:\\/\\/"];
    /// The length of the URL text at the start of `s` that is not `stop`
    /// and not the URL's end.
    fn run(s: &str, stop: Option<char>) -> usize {
        let mut it = s.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            let ends = c.is_whitespace()
                || matches!(c, '"' | '\'' | '<' | '>' | '`')
                || Some(c) == stop
                || (c == '\\'
                    && matches!(it.peek(), Some((_, '"' | 'n' | 'r' | 't' | '\\')) | None));
            if ends {
                return i;
            }
            if c == '\\' {
                it.next();
            }
        }
        s.len()
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = SCHEMES
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, p.len())))
            .min();
        let Some((i, len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i + len]);
        rest = &rest[i + len..];
        let path = run(rest, Some('?'));
        out.push_str(&redact_short_link_id(&rest[..path]));
        rest = &rest[path..];
        if let Some(query) = rest.strip_prefix('?') {
            out.push_str("?[redacted]");
            rest = &query[run(query, None)..];
        }
    }
}

/// [`redact_render_links`] and [`redact_url_queries`]: what capture mode
/// applies to every value it records.
pub fn redact_links(text: &str) -> String {
    redact_url_queries(&redact_render_links(text))
}

/// One artifact to store: its MIME type and its bytes.
#[derive(Debug, Clone)]
pub struct ArtifactObject {
    /// The MIME type it is served with.
    pub mime_type: String,
    /// The bytes, as the worker produced them.
    pub bytes: Arc<[u8]>,
}

/// The kind of link a backend gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkScheme {
    /// `pscat://render/<render_id>/<n>`: an MCP resource, read with
    /// `resources/read` by the principal that made it.
    Pscat,
    /// An `https:` URL anyone holding it can fetch until it expires (a
    /// signed URL; #140).
    Https,
    /// A short `https:` link on this server, `/a/<render_id>/<n>.<ext>`,
    /// that redirects to a signed URL minted when it is opened; anyone
    /// holding it can fetch the object until it expires (#163).
    Short,
}

/// The link to one stored artifact.
#[derive(Debug, Clone)]
pub struct ArtifactLink {
    /// The URI of the result's `resource_link` block.
    pub uri: String,
    /// The URL to fetch it: `structuredContent`'s `download_url`. The same
    /// as `uri` for the memory backend.
    pub download_url: String,
    /// What kind of link it is.
    pub scheme: LinkScheme,
}

/// One call's stored artifacts.
#[derive(Debug, Clone)]
pub struct StoredRender {
    /// The random id of the call's output.
    pub render_id: String,
    /// When the links stop working.
    pub expires_at: SystemTime,
    /// One link per artifact, in the result's order.
    pub links: Vec<ArtifactLink>,
}

impl StoredRender {
    /// `expires_at` as RFC 3339, to the second, in UTC.
    pub fn expires_at_rfc3339(&self) -> String {
        chrono::DateTime::<chrono::Utc>::from(self.expires_at)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }
}

/// One stored artifact, read back.
#[derive(Debug, Clone)]
pub struct ReadArtifact {
    /// Its MIME type.
    pub mime_type: String,
    /// Its bytes.
    pub bytes: Arc<[u8]>,
}

/// A boxed future, as the store's methods return.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Keeps one call's finished artifacts and hands out links to them.
///
/// Implementations never log a link, a render id or the bytes.
pub trait ArtifactStore: Send + Sync {
    /// The backend's name, as `PSCAT_MCP_ARTIFACT_LINKS` names it.
    fn name(&self) -> &'static str;

    /// How long links work.
    fn ttl(&self) -> Duration;

    /// Bytes one principal may have stored at once.
    fn quota_bytes(&self) -> usize;

    /// Store `objects` for `owner` under a new random render id, and return
    /// one link per object. A refusal (the quota, the budget) is a
    /// `results_budget_exceeded` error naming the limit.
    fn put<'a>(
        &'a self,
        owner: &'a str,
        objects: Vec<ArtifactObject>,
    ) -> StoreFuture<'a, Result<StoredRender, RenderError>>;

    /// Artifact `n` (from 1) of `owner`'s render `render_id`, if it exists,
    /// belongs to `owner` and has not expired. A backend that serves no
    /// bytes itself (signed URLs) returns `None`.
    fn read<'a>(
        &'a self,
        owner: &'a str,
        render_id: &'a str,
        n: usize,
    ) -> StoreFuture<'a, Option<ReadArtifact>> {
        let _ = (owner, render_id, n);
        Box::pin(async { None })
    }

    /// Whether this store's links are short links served by
    /// [`Self::redeem`] at `/a/...` (`gcs` with a public origin, #163).
    fn serves_short_links(&self) -> bool {
        false
    }

    /// Answer the short link `path` with `query`: the URL to redirect to,
    /// or why not. A store without short links answers
    /// [`LinkError::NotFound`].
    fn redeem<'a>(
        &'a self,
        path: &'a str,
        query: Option<&'a str>,
    ) -> StoreFuture<'a, Result<String, LinkError>> {
        let _ = (path, query);
        Box::pin(async { Err(LinkError::NotFound) })
    }
}

/// A monotonic clock, injectable for tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The memory backend: the bytes are held by the job store's ledger, under
/// the async-job results budget, with a per-principal quota and a TTL.
pub struct MemoryArtifactStore {
    ledger: Arc<JobStore>,
    ttl: Duration,
    quota: usize,
    clock: Clock,
}

impl std::fmt::Debug for MemoryArtifactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryArtifactStore")
            .field("ttl", &self.ttl)
            .field("quota", &self.quota)
            .finish()
    }
}

impl MemoryArtifactStore {
    /// A store in `ledger`'s budget, with links working for `ttl` and at
    /// most `quota` bytes per principal.
    pub fn new(ledger: Arc<JobStore>, ttl: Duration, quota: usize) -> Self {
        Self::with_clock(ledger, ttl, quota, Arc::new(Instant::now))
    }

    /// [`Self::new`] with `clock` as the time (tests).
    pub fn with_clock(ledger: Arc<JobStore>, ttl: Duration, quota: usize, clock: Clock) -> Self {
        MemoryArtifactStore {
            ledger,
            ttl,
            quota,
            clock,
        }
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn ttl(&self) -> Duration {
        self.ttl
    }

    fn quota_bytes(&self) -> usize {
        self.quota
    }

    fn put<'a>(
        &'a self,
        owner: &'a str,
        objects: Vec<ArtifactObject>,
    ) -> StoreFuture<'a, Result<StoredRender, RenderError>> {
        Box::pin(async move {
            let now = (self.clock)();
            let count = objects.len();
            let id = self
                .ledger
                .put_artifacts(owner, objects, self.quota, now + self.ttl, now)?;
            let links = (1..=count)
                .map(|n| {
                    let uri = render_uri(&id, n);
                    ArtifactLink {
                        download_url: uri.clone(),
                        uri,
                        scheme: LinkScheme::Pscat,
                    }
                })
                .collect();
            Ok(StoredRender {
                render_id: id,
                expires_at: SystemTime::now() + self.ttl,
                links,
            })
        })
    }

    fn read<'a>(
        &'a self,
        owner: &'a str,
        render_id: &'a str,
        n: usize,
    ) -> StoreFuture<'a, Option<ReadArtifact>> {
        Box::pin(async move {
            self.ledger
                .read_artifact(owner, render_id, n, (self.clock)())
                .map(|(mime_type, bytes)| ReadArtifact { mime_type, bytes })
        })
    }
}

/// A `results_budget_exceeded` refusal for the artifact store, naming the
/// limit (`artifact_quota_bytes` or `results_budget_bytes`).
pub(crate) fn store_refusal(
    message: String,
    code: &str,
    limit: usize,
    reached: usize,
) -> RenderError {
    let mut err = RenderError::new(ErrorCode::ResultsBudgetExceeded, message);
    err.input = Some(Box::new(InputDiagnostic {
        code: code.to_string(),
        token: None,
        limit_bytes: Some(limit as u64),
        reached_bytes: Some(reached as u64),
    }));
    err
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::config::{JobsConfig, LimitsConfig};

    /// A clock that moves only when told to.
    fn manual_clock() -> (Clock, Arc<Mutex<Instant>>) {
        let t = Arc::new(Mutex::new(Instant::now()));
        let read = t.clone();
        (Arc::new(move || *read.lock().unwrap()), t)
    }

    fn store(
        budget: usize,
        quota: usize,
        ttl_secs: u64,
    ) -> (MemoryArtifactStore, Arc<Mutex<Instant>>) {
        let ledger = Arc::new(JobStore::new(
            JobsConfig {
                job_results_budget_bytes: budget,
                ..JobsConfig::default()
            },
            &LimitsConfig::default(),
        ));
        let (clock, t) = manual_clock();
        (
            MemoryArtifactStore::with_clock(ledger, Duration::from_secs(ttl_secs), quota, clock),
            t,
        )
    }

    fn object(len: usize) -> ArtifactObject {
        ArtifactObject {
            mime_type: "image/png".into(),
            bytes: (0..len).map(|i| i as u8).collect::<Vec<u8>>().into(),
        }
    }

    fn run<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn uris_round_trip_and_reject_anything_else() {
        let uri = render_uri("abc", 3);
        assert_eq!(uri, "pscat://render/abc/3");
        assert_eq!(parse_render_uri(&uri), Some(("abc", 3)));
        for bad in [
            "pscat://render/abc/0",
            "pscat://render/abc",
            "pscat://render//1",
            "pscat://render/abc/x",
            "pscat://guide/abc/1",
        ] {
            assert_eq!(parse_render_uri(bad), None, "{bad}");
        }
    }

    #[test]
    fn render_links_are_redacted_escaped_or_not() {
        let id = "0b9c2f7e-1a2b-4c3d-8e9f-001122334455";
        let body = format!(
            r#"{{"uri":"pscat://render/{id}/2","x":"pscat:\/\/render\/{id}\/1","g":"pscat://guide/a.md"}}"#
        );
        let out = redact_render_links(&body);
        assert!(!out.contains(id), "{out}");
        assert!(out.contains("pscat://render/[redacted]/2"), "{out}");
        assert!(out.contains("pscat://guide/a.md"), "{out}");
        assert_eq!(redact_render_links("no links"), "no links");
    }

    #[test]
    fn url_queries_are_redacted_escaped_or_not() {
        let url = "https://storage.googleapis.com/b/renders/x/1.png?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Signature=abcdef0123";
        let body = format!(
            r#"{{"a":"{url}","b":"see {url} now","c":"https:\/\/h\/p?X-Goog-Signature=feed\/x"}}"#
        );
        let out = redact_url_queries(&body);
        assert!(
            !out.contains("abcdef0123") && !out.contains("feed"),
            "{out}"
        );
        assert!(!out.contains("X-Goog"), "{out}");
        assert!(
            out.contains(r#""https://storage.googleapis.com/b/renders/x/1.png?[redacted]""#),
            "{out}"
        );
        assert!(out.contains("png?[redacted] now"), "{out}");
        assert!(out.contains(r#"https:\/\/h\/p?[redacted]""#), "{out}");
        // Some encoders escape `&` as `\u0026`: still one query.
        let out = redact_url_queries(r#""https://h/p?a=1\u0026X-Goog-Signature=beef""#);
        assert_eq!(out, r#""https://h/p?[redacted]""#);
        assert_eq!(redact_url_queries("no urls ?a=b"), "no urls ?a=b");
        // A short link: the query and the render id in its path.
        let id = "0b9c2f7e-1a2b-4c3d-8e9f-001122334455";
        let out = redact_url_queries(&format!(
            r#"{{"a":"https://h.test/a/{id}/1.png?e=1&s=tok","b":"https:\/\/h.test\/a\/{id}\/2.svg?e=1"}}"#
        ));
        assert!(!out.contains(id) && !out.contains("tok"), "{out}");
        assert!(
            out.contains("https://h.test/a/[redacted]/1.png?[redacted]"),
            "{out}"
        );
        assert!(
            out.contains(r#"https:\/\/h.test\/a\/[redacted]\/2.svg?[redacted]"#),
            "{out}"
        );
        // Bare ids outside a URL (a job id) are kept.
        assert_eq!(redact_url_queries(id), id);
        assert_eq!(redact_url_queries("https://h/p"), "https://h/p");
        assert_eq!(
            redact_url_queries("http://h/p?q é"),
            "http://h/p?[redacted] é"
        );
    }

    #[test]
    fn auto_follows_the_threshold() {
        assert_eq!(Delivery::Auto.decide(10, 10), Delivery::Inline);
        assert_eq!(Delivery::Auto.decide(11, 10), Delivery::Url);
        assert_eq!(Delivery::Both.decide(0, 10), Delivery::Both);
        for name in Delivery::NAMES {
            assert_eq!(Delivery::parse(name).unwrap().as_str(), name);
        }
        assert_eq!(Delivery::parse("link"), None);
    }

    #[test]
    fn stored_bytes_come_back_to_their_owner_only() {
        let (s, _) = store(1 << 20, 1 << 16, 60);
        let stored = run(s.put("a", vec![object(100), object(7)])).unwrap();
        assert_eq!(stored.links.len(), 2);
        assert_eq!(stored.links[1].uri, render_uri(&stored.render_id, 2));
        assert_eq!(stored.links[1].download_url, stored.links[1].uri);
        // A UUID v4: random, never derived from the input.
        assert_eq!(
            uuid::Uuid::parse_str(&stored.render_id)
                .unwrap()
                .get_version_num(),
            4
        );
        let got = run(s.read("a", &stored.render_id, 1)).unwrap();
        assert_eq!(&got.bytes[..], &object(100).bytes[..]);
        assert_eq!(got.mime_type, "image/png");
        assert_eq!(
            run(s.read("a", &stored.render_id, 2)).unwrap().bytes.len(),
            7
        );
        assert!(run(s.read("a", &stored.render_id, 3)).is_none());
        assert!(run(s.read("b", &stored.render_id, 1)).is_none());
        assert!(run(s.read("a", "no-such-id", 1)).is_none());
        let again = run(s.put("a", vec![object(1)])).unwrap();
        assert_ne!(again.render_id, stored.render_id);
    }

    #[test]
    fn artifacts_expire_after_the_ttl_and_free_their_bytes() {
        let (s, t) = store(1 << 20, 1 << 16, 30);
        let stored = run(s.put("a", vec![object(100)])).unwrap();
        assert_eq!(
            s.ledger.artifact_charged_bytes("a"),
            100 + RENDER_CHARGE_BYTES
        );
        *t.lock().unwrap() += Duration::from_secs(29);
        assert!(run(s.read("a", &stored.render_id, 1)).is_some());
        *t.lock().unwrap() += Duration::from_secs(1);
        assert!(run(s.read("a", &stored.render_id, 1)).is_none());
        assert_eq!(s.ledger.artifact_charged_bytes("a"), 0);
        assert_eq!(s.ledger.charged_bytes(), 0);
    }

    #[test]
    fn the_quota_refuses_and_expiry_makes_room() {
        let quota = 2 * (100 + RENDER_CHARGE_BYTES);
        let (s, t) = store(1 << 20, quota, 30);
        run(s.put("a", vec![object(100)])).unwrap();
        run(s.put("a", vec![object(100)])).unwrap();
        let e = run(s.put("a", vec![object(100)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded);
        let input = e.input.as_deref().unwrap();
        assert_eq!(input.code, "artifact_quota_bytes");
        assert_eq!(input.limit_bytes, Some(quota as u64));
        assert_eq!(
            input.reached_bytes,
            Some(3 * (100 + RENDER_CHARGE_BYTES) as u64)
        );
        // Another principal has its own quota.
        run(s.put("b", vec![object(100)])).unwrap();
        // Once the first two expire, `a` has room again.
        *t.lock().unwrap() += Duration::from_secs(30);
        run(s.put("a", vec![object(100)])).unwrap();
    }

    #[test]
    fn the_shared_budget_bounds_every_principal_together() {
        let one = 100 + RENDER_CHARGE_BYTES;
        let (s, _) = store(2 * one, 2 * one, 30);
        run(s.put("a", vec![object(100)])).unwrap();
        run(s.put("b", vec![object(100)])).unwrap();
        let e = run(s.put("c", vec![object(100)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded);
        assert_eq!(e.input.as_deref().unwrap().code, "results_budget_bytes");
    }

    #[test]
    fn stored_artifacts_count_against_async_job_reservations() {
        let limits = LimitsConfig::default();
        let largest = JobsConfig::max_result_bytes(&limits);
        let ledger = Arc::new(JobStore::new(
            JobsConfig {
                job_results_budget_bytes: largest + 10_000,
                job_results_per_principal_bytes: Some(largest + 10_000),
                ..JobsConfig::default()
            }
            .enabled(),
            &limits,
        ));
        let s = MemoryArtifactStore::new(ledger.clone(), Duration::from_secs(30), 1 << 20);
        run(s.put("a", vec![object(20_000)])).unwrap();
        let e = ledger.insert("b", "render_postscript").unwrap_err();
        assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded);
    }
}
