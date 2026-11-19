//! Short artifact links (#163): `https://<origin>/a/<render_id>/<n>.<ext>?e=<expiry>&s=<token>`.
//!
//! A V4 signed URL is about 700 characters, and signs every query
//! parameter: a client that appends `utm_source=...`, or a model that
//! re-types the 512-hex-digit signature, breaks it. With a public origin
//! (`PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN`) the `gcs` store hands out a short link
//! on this server instead, and `GET /a/...` (no `Authorization`, like the
//! health paths) answers `302` to a V4 signed URL minted at that moment, with
//! a life of [`REDIRECT_URL_SECS`] at most.
//!
//! * **The token** is the first 16 bytes of an HMAC-SHA-256 over a
//!   domain-separated message ([`message`]) of the `render_id`, `n`, the
//!   extension and the expiry `e`, base64url without padding (22
//!   characters). The key is a shared secret read once from a file
//!   (`PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE`), the same on every instance;
//!   a previous secret (`PSCAT_MCP_ARTIFACT_LINK_SECRET_PREVIOUS_FILE`) is
//!   still accepted, for rotation. Tokens are compared in constant time.
//! * **The path is parsed strictly** ([`parse_path`]): a lowercase
//!   hyphenated UUID as minted, a page number from 1 with no leading zero,
//!   and one of the extensions the store uploads. The object name is built
//!   from those three, which the token covers, so no client-chosen text ever
//!   reaches the bucket.
//! * **The query**: the first `e` and the first `s` count; every other
//!   parameter (`utm_*`, ...) is ignored. `s` is read as its leading run of
//!   base64url characters, so a `?utm_source=...` glued onto it still works.
//!
//! Nothing here logs; the secret, a token and a link never reach a log line,
//! an error message or a `Debug` string.

use std::path::Path;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use subtle::ConstantTimeEq as _;

/// The longest a redirect's V4 signed URL works.
pub const REDIRECT_URL_SECS: u64 = 60;

/// The fewest bytes a link secret may have.
pub const MIN_SECRET_BYTES: usize = 32;

/// The path every short link starts with.
pub const ROUTE_PREFIX: &str = "/a/";

/// The highest page number a link may name.
pub const MAX_N: usize = 9999;

/// The extensions the store uploads (`<n>.<ext>`).
pub const EXTENSIONS: [&str; 4] = ["png", "svg", "pdf", "bin"];

/// Bytes of the HMAC kept in a token.
const TOKEN_BYTES: usize = 16;
/// Characters of a token (16 bytes, base64url, no padding).
pub const TOKEN_CHARS: usize = 22;

/// What the message starts with: a name and a version, so a token minted
/// here can never be one for another use of the same key.
const DOMAIN: &[u8] = b"pscat-mcp artifact link v1";

/// One link secret. Its `Debug` never shows it.
#[derive(Clone)]
struct Secret(ring::hmac::Key);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Secret {
    fn new(bytes: &[u8]) -> Self {
        Secret(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, bytes))
    }

    fn tag(&self, link: &LinkTarget, e: u64) -> [u8; TOKEN_BYTES] {
        let full = ring::hmac::sign(&self.0, &message(link, e));
        let mut out = [0u8; TOKEN_BYTES];
        out.copy_from_slice(&full.as_ref()[..TOKEN_BYTES]);
        out
    }
}

/// The message a token signs: [`DOMAIN`], then the `render_id`, `n`, the
/// extension and `e`, each after a NUL (none of them can hold one).
fn message(link: &LinkTarget, e: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + 64);
    m.extend_from_slice(DOMAIN);
    for field in [
        link.render_id.as_str(),
        &link.n.to_string(),
        link.ext,
        &e.to_string(),
    ] {
        m.push(0);
        m.extend_from_slice(field.as_bytes());
    }
    m
}

/// The secret in `bytes`, from the file named by `name`: one trailing
/// newline (`\n` or `\r\n`) taken off, then at least [`MIN_SECRET_BYTES`].
fn secret_from(name: &str, mut bytes: Vec<u8>) -> Result<Secret, String> {
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.ends_with(b"\r") {
            bytes.pop();
        }
    }
    if bytes.len() < MIN_SECRET_BYTES {
        return Err(format!(
            "the file {name} names holds {} bytes; a link secret needs at least \
             {MIN_SECRET_BYTES} (for example `openssl rand -base64 48`)",
            bytes.len()
        ));
    }
    Ok(Secret::new(&bytes))
}

fn read_secret(name: &str, path: &Path) -> Result<Secret, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("reading {name} ({}): {e}", path.display()))?;
    secret_from(name, bytes)
}

/// The object a link names: `<render_id>/<n>.<ext>` under the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    /// The render id: a lowercase hyphenated UUID.
    pub render_id: String,
    /// The artifact number, from 1.
    pub n: usize,
    /// The extension, one of [`EXTENSIONS`].
    pub ext: &'static str,
}

impl LinkTarget {
    /// The object name under `prefix`, as the store uploads it.
    pub fn object_name(&self, prefix: &str) -> String {
        format!("{prefix}{}/{}.{}", self.render_id, self.n, self.ext)
    }
}

/// Why a short link gives no redirect. Each is answered with a fixed
/// body and no detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Not a short link path (`404`).
    NotFound,
    /// No token, or one that does not verify (`403`).
    Forbidden,
    /// A genuine link past its expiry (`410`).
    Expired,
    /// The signed URL could not be minted (IAM or the metadata server
    /// failed; `503`).
    Unavailable,
}

/// The short-link settings: the public origin and the keys.
#[derive(Clone)]
pub struct ShortLinks {
    origin: String,
    current: Secret,
    previous: Option<Secret>,
}

impl std::fmt::Debug for ShortLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShortLinks")
            .field("previous", &self.previous.is_some())
            .finish_non_exhaustive()
    }
}

/// Check a public origin: `https://host[:port]`, with no user, path (a
/// trailing `/` aside), query or fragment. Returns it without the `/`.
pub fn check_origin(value: &str) -> Result<String, String> {
    const NAME: &str = "PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN";
    let bad = || {
        format!(
            "{NAME} must be an https origin with no path, query or fragment (for example \
             https://mcp.example.com), or `direct` for V4 signed URLs"
        )
    };
    let trimmed = value.strip_suffix('/').unwrap_or(value);
    let uri: http::Uri = trimmed.parse().map_err(|_| bad())?;
    let authority = uri.authority().ok_or_else(bad)?;
    let ok = uri.scheme_str() == Some("https")
        && !authority.host().is_empty()
        && !authority.as_str().contains('@')
        && matches!(uri.path(), "" | "/")
        && uri.query().is_none()
        && !trimmed.contains('#')
        && trimmed == format!("https://{authority}");
    if ok {
        Ok(trimmed.to_string())
    } else {
        Err(bad())
    }
}

impl ShortLinks {
    /// Short links on `origin` (already checked) with the secrets in
    /// `current` and, for rotation, `previous`, each read once.
    pub fn load(origin: &str, current: &Path, previous: Option<&Path>) -> Result<Self, String> {
        let current = read_secret("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", current)?;
        let previous = previous
            .map(|p| read_secret("PSCAT_MCP_ARTIFACT_LINK_SECRET_PREVIOUS_FILE", p))
            .transpose()?;
        Ok(ShortLinks {
            origin: origin.trim_end_matches('/').to_string(),
            current,
            previous,
        })
    }

    /// Short links from secret bytes (tests): `current`, then `previous`.
    pub fn from_secrets(
        origin: &str,
        current: &[u8],
        previous: Option<&[u8]>,
    ) -> Result<Self, String> {
        Ok(ShortLinks {
            origin: origin.trim_end_matches('/').to_string(),
            current: secret_from("current", current.to_vec())?,
            previous: previous
                .map(|p| secret_from("previous", p.to_vec()))
                .transpose()?,
        })
    }

    /// The link to `target`, working until `e` (unix seconds).
    pub fn link(&self, target: &LinkTarget, e: u64) -> String {
        let token =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.current.tag(target, e));
        format!(
            "{}{ROUTE_PREFIX}{}/{}.{}?e={e}&s={token}",
            self.origin, target.render_id, target.n, target.ext
        )
    }

    /// Check a request for `path` and `query` at `now`: the object it may
    /// fetch, and how long it still works, or why not. The token is checked
    /// before the expiry, so only a genuine link is ever told it expired.
    pub fn verify(
        &self,
        path: &str,
        query: Option<&str>,
        now: SystemTime,
    ) -> Result<(LinkTarget, Duration), LinkError> {
        let target = parse_path(path).ok_or(LinkError::NotFound)?;
        let (e, token) = parse_query(query.unwrap_or("")).ok_or(LinkError::Forbidden)?;
        let ok_current = self.current.tag(&target, e)[..].ct_eq(&token[..]);
        let ok_previous = self.previous.as_ref().map_or(subtle::Choice::from(0), |p| {
            p.tag(&target, e)[..].ct_eq(&token[..])
        });
        if !bool::from(ok_current | ok_previous) {
            return Err(LinkError::Forbidden);
        }
        let now = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if now >= e {
            return Err(LinkError::Expired);
        }
        Ok((target, Duration::from_secs(e - now)))
    }
}

/// `/a/<render_id>/<n>.<ext>`, strictly: the id as [`uuid::Uuid`] prints
/// it (lowercase, hyphenated), `n` from 1 to [`MAX_N`] with no leading
/// zero or sign, and one of [`EXTENSIONS`]. Nothing is percent-decoded.
pub fn parse_path(path: &str) -> Option<LinkTarget> {
    let rest = path.strip_prefix(ROUTE_PREFIX)?;
    let (id, file) = rest.split_once('/')?;
    let parsed = uuid::Uuid::parse_str(id).ok()?;
    if parsed.hyphenated().to_string() != id {
        return None;
    }
    let (n, ext) = file.split_once('.')?;
    let digits_ok = !n.is_empty()
        && n.len() <= 4
        && n.bytes().all(|b| b.is_ascii_digit())
        && !n.starts_with('0');
    if !digits_ok {
        return None;
    }
    let n: usize = n.parse().ok()?;
    let ext = EXTENSIONS.into_iter().find(|x| *x == ext)?;
    (1..=MAX_N).contains(&n).then(|| LinkTarget {
        render_id: id.to_string(),
        n,
        ext,
    })
}

/// The first `e` (1 to 20 digits) and the first `s` (its leading run of
/// base64url characters, exactly [`TOKEN_CHARS`] long, decoded) of
/// `query`; every other parameter is ignored.
fn parse_query(query: &str) -> Option<(u64, [u8; TOKEN_BYTES])> {
    let mut e = None;
    let mut s = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "e" if e.is_none() => e = Some(v),
            "s" if s.is_none() => s = Some(v),
            _ => {}
        }
    }
    let e = e?;
    if e.is_empty() || e.len() > 20 || !e.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let e: u64 = e.parse().ok()?;
    let s = s?;
    let run = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        .count();
    if run != TOKEN_CHARS {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&s[..run])
        .ok()?;
    bytes.try_into().ok().map(|t| (e, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b9c2f7e-1a2b-4c3d-8e9f-001122334455";
    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const OLD: &[u8] = b"an older secret, also 32 bytes or more";

    fn target(n: usize, ext: &'static str) -> LinkTarget {
        LinkTarget {
            render_id: ID.into(),
            n,
            ext,
        }
    }

    fn links() -> ShortLinks {
        ShortLinks::from_secrets("https://mcp.example.test/", KEY, Some(OLD)).unwrap()
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The path and the query of `link`.
    fn split(link: &str) -> (&str, &str) {
        let rest = link.strip_prefix("https://mcp.example.test").unwrap();
        rest.split_once('?').unwrap()
    }

    #[test]
    fn a_link_is_short_and_verifies_until_it_expires() {
        let l = links();
        let link = l.link(&target(2, "png"), 1_800_000_000);
        let (path, query) = split(&link);
        assert_eq!(path, format!("/a/{ID}/2.png"));
        let token = query.strip_prefix("e=1800000000&s=").unwrap();
        assert_eq!(token.len(), TOKEN_CHARS);
        assert!(link.len() < 110, "{} chars: {link}", link.len());
        let (t, left) = l.verify(path, Some(query), at(1_799_999_000)).unwrap();
        assert_eq!(t, target(2, "png"));
        assert_eq!(left, Duration::from_secs(1000));
        assert_eq!(t.object_name("renders/"), format!("renders/{ID}/2.png"));
        assert_eq!(
            l.verify(path, Some(query), at(1_800_000_000)),
            Err(LinkError::Expired)
        );
    }

    #[test]
    fn extra_parameters_are_ignored_and_a_glued_one_too() {
        let l = links();
        let link = l.link(&target(1, "svg"), 2_000_000_000);
        let (path, query) = split(&link);
        let now = at(1_900_000_000);
        for q in [
            format!("{query}&utm_source=chatgpt.com"),
            format!("utm_source=chatgpt.com&{query}"),
            format!("{query}?utm_source=chatgpt.com"),
            format!("{query}&e=1&s=AAAAAAAAAAAAAAAAAAAAAA"),
        ] {
            assert!(l.verify(path, Some(&q), now).is_ok(), "{q}");
        }
    }

    #[test]
    fn every_changed_field_is_refused() {
        let l = links();
        let link = l.link(&target(1, "png"), 2_000_000_000);
        let (path, query) = split(&link);
        let now = at(1_900_000_000);
        let token = query.split("s=").nth(1).unwrap();
        // One token character changed, in every position.
        for i in 0..token.len() {
            let mut t: Vec<u8> = token.bytes().collect();
            t[i] = if t[i] == b'A' { b'B' } else { b'A' };
            let q = format!("e=2000000000&s={}", String::from_utf8(t).unwrap());
            assert_eq!(
                l.verify(path, Some(&q), now),
                Err(LinkError::Forbidden),
                "{i}"
            );
        }
        let other_id = "0b9c2f7e-1a2b-4c3d-8e9f-001122334456";
        for (p, q) in [
            (format!("/a/{other_id}/1.png"), query.to_string()),
            (format!("/a/{ID}/2.png"), query.to_string()),
            (format!("/a/{ID}/1.svg"), query.to_string()),
            (
                path.to_string(),
                query.replace("e=2000000000", "e=2000000001"),
            ),
            (
                path.to_string(),
                query.replace("e=2000000000", "e=1000000000"),
            ),
            (path.to_string(), "e=2000000000".into()),
            (path.to_string(), format!("s={token}")),
            (path.to_string(), String::new()),
            (path.to_string(), format!("e=2000000000&s={}", &token[..21])),
            (path.to_string(), format!("e=+2000000000&s={token}")),
        ] {
            assert_eq!(
                l.verify(&p, Some(&q), now),
                Err(LinkError::Forbidden),
                "{p}?{q}"
            );
        }
        assert_eq!(l.verify(path, None, now), Err(LinkError::Forbidden));
    }

    #[test]
    fn the_previous_secret_still_verifies_and_another_does_not() {
        let old = ShortLinks::from_secrets("https://h.test", OLD, None).unwrap();
        let link = old.link(&target(1, "pdf"), 2_000_000_000);
        let (path, query) = link
            .strip_prefix("https://h.test")
            .unwrap()
            .split_once('?')
            .unwrap();
        let now = at(1_900_000_000);
        assert!(links().verify(path, Some(query), now).is_ok());
        let unrelated = ShortLinks::from_secrets("https://h.test", KEY, None).unwrap();
        assert_eq!(
            unrelated.verify(path, Some(query), now),
            Err(LinkError::Forbidden)
        );
    }

    #[test]
    fn paths_are_parsed_strictly() {
        assert_eq!(
            parse_path(&format!("/a/{ID}/12.png")),
            Some(target(12, "png"))
        );
        let upper = ID.to_uppercase();
        let simple = ID.replace('-', "");
        for bad in [
            format!("/a/{upper}/1.png"),
            format!("/a/{simple}/1.png"),
            format!("/a/{{{ID}}}/1.png"),
            format!("/a/urn:uuid:{ID}/1.png"),
            format!("/a/{ID}/0.png"),
            format!("/a/{ID}/01.png"),
            format!("/a/{ID}/+1.png"),
            format!("/a/{ID}/10000.png"),
            format!("/a/{ID}/1.PNG"),
            format!("/a/{ID}/1.png.png"),
            format!("/a/{ID}/1.exe"),
            format!("/a/{ID}/1"),
            format!("/a/{ID}/1.png/"),
            format!("/a/{ID}/../1.png"),
            format!("/a/{ID}%2F1.png"),
            format!("/a/../{ID}/1.png"),
            format!("/a/{ID}/x/1.png"),
            format!("/b/{ID}/1.png"),
            "/a/".to_string(),
        ] {
            assert_eq!(parse_path(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_message_is_domain_separated() {
        // Moving bytes across a field boundary changes the message.
        let a = LinkTarget {
            render_id: "x1".into(),
            n: 2,
            ext: "png",
        };
        let b = LinkTarget {
            render_id: "x".into(),
            n: 12,
            ext: "png",
        };
        assert_ne!(message(&a, 3), message(&b, 3));
        assert_ne!(
            message(&target(1, "png"), 23),
            message(&target(12, "png"), 3)
        );
        assert!(message(&a, 3).starts_with(DOMAIN));
        // The same fields under another key, or HMAC of the bare fields,
        // give another tag.
        let k = Secret::new(KEY);
        let bare: Vec<u8> = format!("{ID}1png2000000000").into_bytes();
        let full = ring::hmac::sign(&k.0, &bare);
        assert_ne!(
            &full.as_ref()[..16],
            &k.tag(&target(1, "png"), 2_000_000_000)[..]
        );
        assert_ne!(
            Secret::new(OLD).tag(&target(1, "png"), 1),
            k.tag(&target(1, "png"), 1)
        );
    }

    #[test]
    fn secrets_are_checked_and_never_shown() {
        assert!(ShortLinks::from_secrets("https://h", &[b'x'; 31], None).is_err());
        // One trailing newline is not counted.
        let mut k = vec![b'x'; 31];
        k.push(b'\n');
        assert!(ShortLinks::from_secrets("https://h", &k, None).is_err());
        assert!(ShortLinks::from_secrets("https://h", &[b'x'; 32], None).is_ok());
        let e = ShortLinks::from_secrets("https://h", KEY, Some(b"short")).unwrap_err();
        assert!(e.contains("32") && !e.contains("short"), "{e}");
        // A file with and without its newline is the same key.
        let mut nl = KEY.to_vec();
        nl.extend_from_slice(b"\r\n");
        let a = ShortLinks::from_secrets("https://h", KEY, None).unwrap();
        let b = ShortLinks::from_secrets("https://h", &nl, None).unwrap();
        assert_eq!(a.link(&target(1, "png"), 5), b.link(&target(1, "png"), 5));
        let d = format!("{:?}", links());
        assert!(!d.contains("0123456789") && !d.contains("example"), "{d}");
    }

    #[test]
    fn origins_are_checked() {
        for (ok, want) in [
            ("https://mcp.example.com", "https://mcp.example.com"),
            ("https://mcp.example.com/", "https://mcp.example.com"),
            (
                "https://mcp.example.com:8443",
                "https://mcp.example.com:8443",
            ),
        ] {
            assert_eq!(check_origin(ok).as_deref(), Ok(want), "{ok}");
        }
        for bad in [
            "http://mcp.example.com",
            "http://127.0.0.1:8080",
            "https://mcp.example.com/mcp",
            "https://mcp.example.com?x=1",
            "https://mcp.example.com#f",
            "https://user@mcp.example.com",
            "mcp.example.com",
            "https://",
            "",
        ] {
            assert!(check_origin(bad).is_err(), "{bad}");
        }
    }
}
