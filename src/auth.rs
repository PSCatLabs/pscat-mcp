//! Request authentication for the HTTP transport.
//!
//! The boundary is the [`Authenticator`] trait: the HTTP layer calls it with
//! the request head before anything reaches the MCP service, and either gets
//! a [`Principal`] (inserted into the request extensions) or an
//! [`AuthError`] (answered with `401`). Two implementations exist today:
//!
//! * [`BearerTokenAuth`] — static bearer tokens from configuration, stored
//!   only as SHA-256 digests and compared in constant time.
//! * [`NoAuth`] — accepts everything; constructed only for loopback
//!   development (see `http::authenticator`).
//!
//! # Adding OAuth later (design only — not implemented)
//!
//! The MCP authorization spec makes the server an OAuth 2.1 *resource
//! server*. Plugging that in needs no change to the routing code:
//!
//! 1. An `OAuthAuthenticator` implementing [`Authenticator`] that validates
//!    the presented access token — either locally as a JWT (issuer, audience
//!    = this server's canonical URI per RFC 8707, expiry, signature against
//!    the authorization server's JWKS, cached and refreshed) or remotely via
//!    token introspection (RFC 7662, with a short positive cache). The
//!    resulting [`Principal`] would carry the subject and granted scopes.
//! 2. A public route `GET /.well-known/oauth-protected-resource` serving
//!    Protected Resource Metadata (RFC 9728): `resource`,
//!    `authorization_servers`, `scopes_supported`, `bearer_methods_supported`.
//! 3. The 401 challenge gaining the metadata pointer:
//!    `WWW-Authenticate: Bearer resource_metadata="https://host/.well-known/oauth-protected-resource"`
//!    (and `error="invalid_token"` / `insufficient_scope` with `403` where
//!    appropriate). [`AuthError::challenge`] is the single place that header
//!    is built, so an OAuth authenticator would override it via
//!    [`Authenticator::challenge`].
//!
//! Static tokens and OAuth could also be chained (try one, then the other)
//! by a small composite `Authenticator`.

use sha2::{Digest, Sha256};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

/// Who made an authenticated request. Never contains secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable, loggable identifier, e.g. `token#0` or `anonymous`.
    pub id: String,
}

impl Principal {
    /// The principal used when authentication is disabled.
    pub fn anonymous() -> Self {
        Principal {
            id: "anonymous".into(),
        }
    }
}

/// Why a request was not authenticated. Messages never echo the credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No `Authorization` header.
    #[error("missing Authorization header")]
    Missing,
    /// The header is not `Bearer <token>` (other scheme, empty, non-UTF-8).
    #[error("malformed Authorization header; expected `Bearer <token>`")]
    Malformed,
    /// A bearer token was presented but is not accepted.
    #[error("invalid bearer token")]
    InvalidToken,
}

impl AuthError {
    /// Stable code for the JSON error body.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing_credentials",
            AuthError::Malformed => "malformed_credentials",
            AuthError::InvalidToken => "invalid_token",
        }
    }
}

/// Authenticates a request from its head. Implementations must be cheap and
/// must not log credentials.
pub trait Authenticator: Send + Sync + 'static {
    /// Accept or reject the request.
    fn authenticate(&self, parts: &http::request::Parts) -> Result<Principal, AuthError>;

    /// The `WWW-Authenticate` value sent with a `401`. An OAuth
    /// authenticator would add `resource_metadata="…"` here (RFC 9728).
    fn challenge(&self, _error: &AuthError) -> String {
        r#"Bearer realm="pscat-mcp""#.to_string()
    }

    /// Short description for startup logs (no secrets).
    fn describe(&self) -> String;
}

/// Accepts every request. Only for loopback development.
#[derive(Debug, Default)]
pub struct NoAuth;

impl Authenticator for NoAuth {
    fn authenticate(&self, _parts: &http::request::Parts) -> Result<Principal, AuthError> {
        Ok(Principal::anonymous())
    }

    fn describe(&self) -> String {
        "none (unauthenticated, loopback only)".into()
    }
}

/// Static bearer tokens, kept only as SHA-256 digests.
///
/// Every request hashes the presented token and compares the digest with
/// every configured digest in constant time, without exiting early, so
/// timing reveals neither which token matched nor how much of it did.
pub struct BearerTokenAuth {
    digests: Vec<[u8; 32]>,
}

impl std::fmt::Debug for BearerTokenAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerTokenAuth")
            .field("tokens", &self.digests.len())
            .finish()
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl BearerTokenAuth {
    /// Accept any of `tokens`. Empty strings are ignored; the caller decides
    /// whether an empty set is acceptable (it accepts nothing).
    pub fn new<I, S>(tokens: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let digests = tokens
            .into_iter()
            .filter(|t| !t.as_ref().is_empty())
            .map(|t| digest(t.as_ref()))
            .collect();
        BearerTokenAuth { digests }
    }

    /// Number of configured tokens.
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    /// Whether no token is configured.
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// Index of the configured token equal to `token`, compared in
    /// constant time over all configured digests.
    fn find(&self, token: &str) -> Option<usize> {
        let presented = digest(token);
        let mut matched = Choice::from(0u8);
        let mut index = 0u32;
        for (i, d) in self.digests.iter().enumerate() {
            let eq = d.ct_eq(&presented);
            matched |= eq;
            // Branch-free select of the matching index.
            index = u32::conditional_select(&index, &(i as u32), eq);
        }
        bool::from(matched).then_some(index as usize)
    }
}

/// Extract the token from `Authorization: Bearer <token>`. The scheme is
/// matched case-insensitively (RFC 7235).
pub fn bearer_token(headers: &http::HeaderMap) -> Result<&str, AuthError> {
    let value = headers
        .get(http::header::AUTHORIZATION)
        .ok_or(AuthError::Missing)?;
    let value = value.to_str().map_err(|_| AuthError::Malformed)?;
    let (scheme, token) = value.trim().split_once(' ').ok_or(AuthError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Malformed);
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(AuthError::Malformed);
    }
    Ok(token)
}

impl Authenticator for BearerTokenAuth {
    fn authenticate(&self, parts: &http::request::Parts) -> Result<Principal, AuthError> {
        let token = bearer_token(&parts.headers)?;
        match self.find(token) {
            Some(i) => Ok(Principal {
                id: format!("token#{i}"),
            }),
            None => Err(AuthError::InvalidToken),
        }
    }

    fn describe(&self) -> String {
        format!("bearer ({} token(s))", self.digests.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(auth: Option<&str>) -> http::request::Parts {
        let mut b = http::Request::builder().uri("/mcp");
        if let Some(a) = auth {
            b = b.header(http::header::AUTHORIZATION, a);
        }
        b.body(()).unwrap().into_parts().0
    }

    fn auth() -> BearerTokenAuth {
        BearerTokenAuth::new(["alpha-token", "", "beta-token"])
    }

    #[test]
    fn matches_each_configured_token() {
        let a = auth();
        assert_eq!(a.len(), 2);
        assert_eq!(
            a.authenticate(&parts(Some("Bearer alpha-token")))
                .unwrap()
                .id,
            "token#0"
        );
        assert_eq!(
            a.authenticate(&parts(Some("Bearer beta-token")))
                .unwrap()
                .id,
            "token#1"
        );
        // Scheme is case-insensitive; surrounding whitespace is tolerated.
        assert_eq!(
            a.authenticate(&parts(Some("bearer  beta-token ")))
                .unwrap()
                .id,
            "token#1"
        );
    }

    #[test]
    fn missing_header() {
        assert_eq!(auth().authenticate(&parts(None)), Err(AuthError::Missing));
    }

    #[test]
    fn wrong_scheme_or_empty() {
        let a = auth();
        for h in [
            "Basic YWxwaGEtdG9rZW4=",
            "alpha-token",
            "Bearer",
            "Bearer   ",
            "Token alpha-token",
        ] {
            assert_eq!(
                a.authenticate(&parts(Some(h))),
                Err(AuthError::Malformed),
                "{h}"
            );
        }
    }

    #[test]
    fn wrong_token() {
        let a = auth();
        for h in [
            "Bearer alpha",
            "Bearer alpha-token2",
            "Bearer ALPHA-TOKEN",
            "Bearer x",
        ] {
            assert_eq!(
                a.authenticate(&parts(Some(h))),
                Err(AuthError::InvalidToken),
                "{h}"
            );
        }
    }

    #[test]
    fn empty_token_set_accepts_nothing() {
        let a = BearerTokenAuth::new(Vec::<String>::new());
        assert!(a.is_empty());
        assert_eq!(
            a.authenticate(&parts(Some("Bearer anything"))),
            Err(AuthError::InvalidToken)
        );
    }

    #[test]
    fn debug_and_describe_hide_tokens() {
        let a = auth();
        let shown = format!("{a:?} {}", a.describe());
        assert!(
            !shown.contains("alpha") && !shown.contains("beta"),
            "{shown}"
        );
    }

    #[test]
    fn no_auth_is_anonymous() {
        assert_eq!(
            NoAuth.authenticate(&parts(None)).unwrap(),
            Principal::anonymous()
        );
    }
}
