//! Request authentication for the HTTP transport.
//!
//! The HTTP layer calls an [`Authenticator`] with the request head before
//! anything reaches the MCP service, and either gets a [`Principal`]
//! (inserted into the request extensions) or a [`Denial`] (answered with its
//! status, `401` today, and a `WWW-Authenticate` challenge).
//!
//! # Several methods side by side
//!
//! An authentication *method* is one way of proving who the caller is: a
//! static bearer token, an OAuth access token, or none at all. Each method
//! implements [`AuthMethod`] and answers one of three [`Outcome`]s for a
//! request:
//!
//! * [`Outcome::Accepted`]: the request carries a credential of this kind
//!   and it is valid; the [`Principal`] says who it is.
//! * [`Outcome::Rejected`]: the request carries a credential this method
//!   handles, and it is not valid (or not allowed).
//! * [`Outcome::NotApplicable`]: the request carries no credential of this
//!   kind.
//!
//! [`AuthChain`] tries the enabled methods in their configured order and
//! admits the request as soon as one accepts it. A *fallback* method
//! ([`AuthMethod::is_fallback`], meant for the explicit no-auth mode) is
//! consulted only when every other method found no credential at all, so a
//! credential that was presented and rejected stays rejected, and a valid
//! token is never downgraded to the fallback's principal, whatever the order
//! of the list. When nothing accepts, the denial carries the most severe
//! rejection (a `403` over a `401`, then the first in order), or
//! [`AuthError::Missing`] when no method found a credential, and a
//! `WWW-Authenticate` header that merges every method's challenge (see
//! [`Challenges`]).
//!
//! Any single [`AuthMethod`] is also an [`Authenticator`] (a chain of one),
//! so `Arc::new(BearerTokenAuth::new(..))` works wherever an authenticator
//! is expected.
//!
//! Methods today:
//!
//! * [`BearerTokenAuth`] (`bearer`): static bearer tokens from
//!   configuration, stored only as SHA-256 digests and compared in constant
//!   time.
//! * [`NoAuth`] (`none`): accepts a request that carries no credential, as
//!   the principal `anonymous`. Plain, it is the loopback development mode
//!   (`--allow-unauthenticated`, see `http::authenticator`). Guarded (see
//!   [`crate::auth_none`]: an expiry, a source allowlist and a per-client
//!   rate limit), it is the explicit `none` of `PSCAT_MCP_AUTH`, for
//!   staging, and may run off loopback.
//!
//! `oauth` (an OAuth 2.1 resource server, issue #119) plugs in as a further
//! [`AuthMethod`]; until then the configuration refuses to start with it.
//!
//! # Rules for a method
//!
//! * **Principal ids are disjoint across methods.** Job ownership and the
//!   per-principal budgets compare only [`Principal::id`]. `bearer` keeps its
//!   bare `token#N` ids (the token's position in the configured list); every
//!   other method prefixes its ids with its name (`oauth:<subject>`), and
//!   only the fallback uses `anonymous`.
//! * **Never log or echo a credential**, in a [`Principal`], an
//!   [`AuthError`] message, a challenge or [`AuthMethod::summary`].
//! * **Look only at the request head.** The body is untrusted input and is
//!   not read until a method has accepted the request.
//! * [`AuthMethod::check`] returns a boxed future so a method can do I/O
//!   (an OAuth method refreshing its cached JWKS); keep it bounded in time.
//!
//! # Adding OAuth (issue #119)
//!
//! An `oauth` method validates the access token from
//! `Authorization: Bearer …` (as a JWT against the authorization server's
//! cached JWKS: issuer, audience = this server's canonical URI per RFC 8707,
//! expiry, scope), answers [`Outcome::NotApplicable`] without a bearer
//! token, and contributes
//! `Challenge::new("Bearer").param("resource_metadata", "https://host/.well-known/oauth-protected-resource")`
//! (plus `error="invalid_token"` when it rejected the token) from
//! [`AuthMethod::challenges`]. Both `bearer` and `oauth` use the `Bearer`
//! scheme, so the merged header is one challenge:
//! `Bearer realm="pscat-mcp", resource_metadata="…"`. A user who signs in
//! but is not allowed gets a new [`AuthError`] variant whose
//! [`AuthError::status`] is `403`. The Protected Resource Metadata route
//! (RFC 9728) is served by the HTTP layer, outside the authenticated `/mcp`.

use std::future::Future;
use std::pin::Pin;

use crate::auth_none::NoneGuard;
use http::StatusCode;
use sha2::{Digest, Sha256};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

/// The future an [`AuthMethod`] or [`Authenticator`] returns.
pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Who made an authenticated request. Never contains secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable, loggable identifier, e.g. `token#0` or `anonymous`. Unique
    /// across methods (see the module docs): job ownership compares only
    /// this.
    pub id: String,
    /// The [`AuthMethod::name`] of the method that accepted the request,
    /// e.g. `bearer`; logged as `auth_method`.
    pub method: &'static str,
}

impl Principal {
    /// A principal `id` accepted by the method named `method`.
    pub fn new(method: &'static str, id: impl Into<String>) -> Self {
        Principal {
            id: id.into(),
            method,
        }
    }

    /// The principal used when authentication is disabled.
    pub fn anonymous() -> Self {
        Principal::new(NoAuth::NAME, "anonymous")
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
    /// The no-auth mode refuses this source address (outside the allowlist,
    /// or not determinable). `403`.
    #[error("requests from this source are not allowed")]
    SourceNotAllowed,
    /// The no-auth mode's per-client rate limit is used up. `429`, with a
    /// `Retry-After` of `retry_after_secs`.
    #[error("rate limit exceeded; retry in {retry_after_secs} s")]
    RateLimited {
        /// Whole seconds until the client's window ends, at least 1.
        retry_after_secs: u64,
    },
    /// The no-auth mode has passed its expiry (`PSCAT_MCP_AUTH_NONE_UNTIL`).
    /// `403`.
    #[error("unauthenticated access has expired; present a credential")]
    AccessExpired,
}

impl AuthError {
    /// Stable code for the JSON error body.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing_credentials",
            AuthError::Malformed => "malformed_credentials",
            AuthError::InvalidToken => "invalid_token",
            AuthError::SourceNotAllowed => "source_not_allowed",
            AuthError::RateLimited { .. } => "rate_limited",
            AuthError::AccessExpired => "unauthenticated_access_expired",
        }
    }

    /// The HTTP status of the answer: `401` for a missing or bad credential,
    /// `403` for a caller the method refuses (the no-auth mode's source
    /// allowlist and expiry; an OAuth user not on the allowlist adds a
    /// variant likewise), `429` for a rate limit.
    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::Missing | AuthError::Malformed | AuthError::InvalidToken => {
                StatusCode::UNAUTHORIZED
            }
            AuthError::SourceNotAllowed | AuthError::AccessExpired => StatusCode::FORBIDDEN,
            AuthError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    /// The `Retry-After` of the answer, in seconds, when it has one.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            AuthError::RateLimited { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        }
    }
}

/// One method's answer for one request. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A valid credential of this method's kind.
    Accepted(Principal),
    /// A credential this method handles, not valid or not allowed.
    Rejected(AuthError),
    /// No credential of this method's kind.
    NotApplicable,
}

/// One `WWW-Authenticate` challenge: an auth scheme and its parameters,
/// e.g. `Bearer realm="pscat-mcp"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    scheme: String,
    params: Vec<(String, String)>,
}

impl Challenge {
    /// A challenge for `scheme` (e.g. `Bearer`) with no parameters yet.
    pub fn new(scheme: impl Into<String>) -> Self {
        Challenge {
            scheme: scheme.into(),
            params: Vec::new(),
        }
    }

    /// Add the parameter `name="value"`. The value is sent as a quoted
    /// string, with `"` and `\` escaped.
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), value.into()));
        self
    }

    /// The auth scheme.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    fn render(&self, out: &mut String) {
        out.push_str(&self.scheme);
        for (i, (name, value)) in self.params.iter().enumerate() {
            out.push_str(if i == 0 { " " } else { ", " });
            out.push_str(name);
            out.push_str("=\"");
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
    }
}

/// The challenges of a `401`, merged by scheme.
///
/// Challenges of the same scheme (compared case-insensitively) become one,
/// in the order first seen, so `bearer` and `oauth`, which both use
/// `Bearer`, give `Bearer realm="pscat-mcp", resource_metadata="…"`. A
/// parameter already present on that scheme keeps its first value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Challenges(Vec<Challenge>);

impl Challenges {
    /// Merge `challenge` in.
    pub fn add(&mut self, challenge: Challenge) {
        match self
            .0
            .iter_mut()
            .find(|c| c.scheme.eq_ignore_ascii_case(&challenge.scheme))
        {
            Some(existing) => {
                for (name, value) in challenge.params {
                    if !existing
                        .params
                        .iter()
                        .any(|(n, _)| n.eq_ignore_ascii_case(&name))
                    {
                        existing.params.push((name, value));
                    }
                }
            }
            None => self.0.push(challenge),
        }
    }

    /// The `WWW-Authenticate` value (challenges separated by `, `, as RFC
    /// 9110 allows), or `None` when there is no challenge.
    pub fn header_value(&self) -> Option<String> {
        let mut out = String::new();
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            c.render(&mut out);
        }
        (!out.is_empty()).then_some(out)
    }
}

/// One way of authenticating a request. See the module docs for the rules a
/// method follows.
pub trait AuthMethod: Send + Sync + 'static {
    /// The method's name as configured in `PSCAT_MCP_AUTH` and logged as
    /// `auth_method`: `bearer`, `oauth`, `none`.
    fn name(&self) -> &'static str;

    /// Judge the request from its head alone.
    fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome>;

    /// The challenges this method adds to a denial. `rejection` is this
    /// method's own rejection of the request, if it rejected it (an OAuth
    /// method adds `error="invalid_token"` then). Added whether or not this
    /// method rejected the request, so a client learns every way in.
    fn challenges(&self, _rejection: Option<&AuthError>) -> Vec<Challenge> {
        Vec::new()
    }

    /// Whether this method accepts requests that no other method recognised
    /// (the no-auth mode). A fallback is consulted only when every
    /// non-fallback method answered [`Outcome::NotApplicable`].
    fn is_fallback(&self) -> bool {
        false
    }

    /// Short description for startup logs (no secrets).
    fn summary(&self) -> String;
}

/// A request that was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// The reported error: the most severe rejection, or
    /// [`AuthError::Missing`] when no method found a credential.
    pub error: AuthError,
    /// The `WWW-Authenticate` value, if any method offers a challenge.
    pub challenge: Option<String>,
    /// The methods that rejected the request (empty when none found a
    /// credential), in configured order; logged as `rejected_by`.
    pub rejected_by: Vec<&'static str>,
}

/// What the HTTP layer calls for every `/mcp` request. Implemented by
/// [`AuthChain`] and by every single [`AuthMethod`]. Implementations must be
/// cheap and must not log credentials.
pub trait Authenticator: Send + Sync + 'static {
    /// Admit the request, with the caller's principal, or deny it.
    fn admit<'a>(
        &'a self,
        parts: &'a http::request::Parts,
    ) -> AuthFuture<'a, Result<Principal, Denial>>;

    /// The enabled methods' names, in order (for logs).
    fn methods(&self) -> Vec<&'static str>;

    /// Short description for startup logs (no secrets).
    fn describe(&self) -> String;
}

/// Try `methods` in order; see the module docs.
async fn decide(
    methods: &[&dyn AuthMethod],
    parts: &http::request::Parts,
) -> Result<Principal, Denial> {
    let mut rejections: Vec<(usize, AuthError)> = Vec::new();
    for fallback in [false, true] {
        // A fallback only runs when no other method saw a credential.
        if fallback && !rejections.is_empty() {
            break;
        }
        for (i, method) in methods.iter().enumerate() {
            if method.is_fallback() != fallback {
                continue;
            }
            match method.check(parts).await {
                Outcome::Accepted(mut principal) => {
                    principal.method = method.name();
                    return Ok(principal);
                }
                Outcome::Rejected(error) => rejections.push((i, error)),
                Outcome::NotApplicable => {}
            }
        }
    }
    rejections.sort_by_key(|(i, _)| *i);
    // The most severe status wins; among equals, the first in order.
    let error = rejections
        .iter()
        .fold(None::<&AuthError>, |best, (_, e)| match best {
            Some(b) if b.status().as_u16() >= e.status().as_u16() => Some(b),
            _ => Some(e),
        })
        .cloned()
        .unwrap_or(AuthError::Missing);
    let mut challenges = Challenges::default();
    for (i, method) in methods.iter().enumerate() {
        let own = rejections.iter().find(|(j, _)| *j == i).map(|(_, e)| e);
        for c in method.challenges(own) {
            challenges.add(c);
        }
    }
    Err(Denial {
        error,
        challenge: challenges.header_value(),
        rejected_by: rejections.iter().map(|(i, _)| methods[*i].name()).collect(),
    })
}

/// Several methods side by side; a request passes if any accepts it. See
/// the module docs for the order and the fallback rule.
pub struct AuthChain {
    methods: Vec<Box<dyn AuthMethod>>,
}

impl std::fmt::Debug for AuthChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthChain")
            .field("methods", &Authenticator::methods(self))
            .finish()
    }
}

impl AuthChain {
    /// The methods in the order they are tried. An empty chain admits
    /// nothing; the configuration refuses an empty list before this.
    pub fn new(methods: Vec<Box<dyn AuthMethod>>) -> Self {
        AuthChain { methods }
    }
}

impl Authenticator for AuthChain {
    fn admit<'a>(
        &'a self,
        parts: &'a http::request::Parts,
    ) -> AuthFuture<'a, Result<Principal, Denial>> {
        Box::pin(async move {
            let methods: Vec<&dyn AuthMethod> = self.methods.iter().map(|m| m.as_ref()).collect();
            decide(&methods, parts).await
        })
    }

    fn methods(&self) -> Vec<&'static str> {
        self.methods.iter().map(|m| m.name()).collect()
    }

    fn describe(&self) -> String {
        self.methods
            .iter()
            .map(|m| m.summary())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl<M: AuthMethod> Authenticator for M {
    fn admit<'a>(
        &'a self,
        parts: &'a http::request::Parts,
    ) -> AuthFuture<'a, Result<Principal, Denial>> {
        Box::pin(async move { decide(&[self as &dyn AuthMethod], parts).await })
    }

    fn methods(&self) -> Vec<&'static str> {
        vec![self.name()]
    }

    fn describe(&self) -> String {
        self.summary()
    }
}

/// Accepts a request that carries no credential, as `anonymous`.
///
/// [`NoAuth::default`] accepts everything (loopback development only);
/// [`NoAuth::guarded`] checks the guard rails of [`crate::auth_none`] first.
/// Either way it is a fallback: it is consulted only when no other method
/// found a credential, so a bad token is never let in as anonymous.
#[derive(Debug, Default)]
pub struct NoAuth {
    guard: Option<NoneGuard>,
}

impl NoAuth {
    /// The method name, logged as `auth_method`.
    pub const NAME: &'static str = "none";

    /// The explicit no-auth mode, behind `guard`.
    pub fn guarded(guard: NoneGuard) -> Self {
        NoAuth { guard: Some(guard) }
    }

    /// The anonymous principal, unless the guard rails refuse the request.
    pub fn authenticate(&self, parts: &http::request::Parts) -> Result<Principal, AuthError> {
        if let Some(guard) = &self.guard {
            guard.check(parts)?;
        }
        Ok(Principal::anonymous())
    }
}

impl AuthMethod for NoAuth {
    fn name(&self) -> &'static str {
        NoAuth::NAME
    }

    fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome> {
        let outcome = match self.authenticate(parts) {
            Ok(p) => Outcome::Accepted(p),
            Err(e) => Outcome::Rejected(e),
        };
        Box::pin(std::future::ready(outcome))
    }

    fn is_fallback(&self) -> bool {
        true
    }

    fn summary(&self) -> String {
        match &self.guard {
            Some(guard) => guard.summary(),
            None => "none (unauthenticated, loopback only)".into(),
        }
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
    /// The method name, logged as `auth_method`.
    pub const NAME: &'static str = "bearer";

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

    /// The principal for `Authorization: Bearer <token>`: `token#N` for the
    /// N-th configured token.
    pub fn authenticate(&self, parts: &http::request::Parts) -> Result<Principal, AuthError> {
        let token = bearer_token(&parts.headers)?;
        match self.find(token) {
            Some(i) => Ok(Principal::new(BearerTokenAuth::NAME, format!("token#{i}"))),
            None => Err(AuthError::InvalidToken),
        }
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

impl AuthMethod for BearerTokenAuth {
    fn name(&self) -> &'static str {
        BearerTokenAuth::NAME
    }

    /// No `Authorization` header is not applicable; any other header this
    /// method cannot use (another scheme, an empty token) is a rejection,
    /// as before (`malformed_credentials`).
    fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome> {
        let outcome = match self.authenticate(parts) {
            Ok(p) => Outcome::Accepted(p),
            Err(AuthError::Missing) => Outcome::NotApplicable,
            Err(e) => Outcome::Rejected(e),
        };
        Box::pin(std::future::ready(outcome))
    }

    /// `Bearer realm="pscat-mcp"`, whatever the error.
    fn challenges(&self, _rejection: Option<&AuthError>) -> Vec<Challenge> {
        vec![Challenge::new("Bearer").param("realm", "pscat-mcp")]
    }

    fn summary(&self) -> String {
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
            NoAuth::default().authenticate(&parts(None)).unwrap(),
            Principal::anonymous()
        );
    }

    // ------------------------------------------------ several methods

    fn block_on<T>(f: AuthFuture<'_, T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn parts_with(headers: &[(&str, &str)]) -> http::request::Parts {
        let mut b = http::Request::builder().uri("/mcp");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    /// A test-only method: `X-Fake-User: <name>` is accepted as
    /// `fake:<name>` unless the name is `mallory`, which is rejected (with
    /// `reject_status`'s error); no header is not applicable.
    struct Fake {
        fallback: bool,
    }

    impl AuthMethod for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome> {
            let outcome = match parts.headers.get("x-fake-user").map(|v| v.to_str()) {
                None => Outcome::NotApplicable,
                Some(Ok("mallory")) => Outcome::Rejected(AuthError::InvalidToken),
                Some(Ok(name)) => {
                    Outcome::Accepted(Principal::new("ignored", format!("fake:{name}")))
                }
                Some(Err(_)) => Outcome::Rejected(AuthError::Malformed),
            };
            Box::pin(std::future::ready(outcome))
        }
        fn challenges(&self, rejection: Option<&AuthError>) -> Vec<Challenge> {
            let c = Challenge::new("Fake").param("realm", "test");
            vec![match rejection {
                Some(e) => c.param("error", e.code()),
                None => c,
            }]
        }
        fn is_fallback(&self) -> bool {
            self.fallback
        }
        fn summary(&self) -> String {
            "fake".into()
        }
    }

    fn chain(methods: Vec<Box<dyn AuthMethod>>) -> AuthChain {
        AuthChain::new(methods)
    }

    fn bearer_and_fake() -> AuthChain {
        chain(vec![Box::new(auth()), Box::new(Fake { fallback: false })])
    }

    #[test]
    fn a_single_method_is_a_chain_of_one() {
        let a = auth();
        let ok = block_on(a.admit(&parts(Some("Bearer beta-token")))).unwrap();
        assert_eq!(ok, Principal::new("bearer", "token#1"));
        for (h, error) in [
            (None, AuthError::Missing),
            (Some("Basic YWxwaGEtdG9rZW4="), AuthError::Malformed),
            (Some("Bearer nope"), AuthError::InvalidToken),
        ] {
            let d = block_on(a.admit(&parts(h))).unwrap_err();
            assert_eq!(d.error, error, "{h:?}");
            assert_eq!(d.challenge.as_deref(), Some(r#"Bearer realm="pscat-mcp""#));
            let rejected: &[&str] = if h.is_some() { &["bearer"] } else { &[] };
            assert_eq!(d.rejected_by, rejected, "{h:?}");
        }
        assert_eq!(Authenticator::methods(&a), ["bearer"]);
        // The chain-of-one and a one-method chain answer alike.
        let c = chain(vec![Box::new(auth())]);
        for h in [None, Some("Basic x"), Some("Bearer nope")] {
            assert_eq!(
                block_on(c.admit(&parts(h))),
                block_on(a.admit(&parts(h))),
                "{h:?}"
            );
        }
        assert_eq!(c.describe(), a.describe());
    }

    #[test]
    fn the_fake_method_alone() {
        let f = Fake { fallback: false };
        assert_eq!(
            block_on(f.admit(&parts_with(&[("x-fake-user", "alice")]))).unwrap(),
            Principal::new("fake", "fake:alice"),
            "the chain stamps the method's own name"
        );
        let d = block_on(f.admit(&parts_with(&[]))).unwrap_err();
        assert_eq!(d.error, AuthError::Missing);
        assert_eq!(d.challenge.as_deref(), Some(r#"Fake realm="test""#));
        let d = block_on(f.admit(&parts_with(&[("x-fake-user", "mallory")]))).unwrap_err();
        assert_eq!(d.error, AuthError::InvalidToken);
        assert_eq!(
            d.challenge.as_deref(),
            Some(r#"Fake realm="test", error="invalid_token""#)
        );
        // A bearer token means nothing to it.
        let d = block_on(f.admit(&parts(Some("Bearer alpha-token")))).unwrap_err();
        assert_eq!(d.error, AuthError::Missing);
    }

    #[test]
    fn any_method_accepting_admits_the_request() {
        let c = bearer_and_fake();
        assert_eq!(Authenticator::methods(&c), ["bearer", "fake"]);
        assert_eq!(c.describe(), "bearer (2 token(s)), fake");
        // Each method on its own credential.
        assert_eq!(
            block_on(c.admit(&parts(Some("Bearer alpha-token")))).unwrap(),
            Principal::new("bearer", "token#0")
        );
        assert_eq!(
            block_on(c.admit(&parts_with(&[("x-fake-user", "alice")]))).unwrap(),
            Principal::new("fake", "fake:alice")
        );
        // Bearer rejects a wrong token; the fake method accepts its header.
        assert_eq!(
            block_on(c.admit(&parts_with(&[
                ("authorization", "Bearer wrong"),
                ("x-fake-user", "alice"),
            ])))
            .unwrap(),
            Principal::new("fake", "fake:alice")
        );
        // The fake method rejects; bearer accepts.
        assert_eq!(
            block_on(c.admit(&parts_with(&[
                ("authorization", "Bearer beta-token"),
                ("x-fake-user", "mallory"),
            ])))
            .unwrap(),
            Principal::new("bearer", "token#1")
        );
        // Both valid: the first in order wins.
        assert_eq!(
            block_on(c.admit(&parts_with(&[
                ("authorization", "Bearer beta-token"),
                ("x-fake-user", "alice"),
            ])))
            .unwrap()
            .method,
            "bearer"
        );
        let reversed = chain(vec![Box::new(Fake { fallback: false }), Box::new(auth())]);
        assert_eq!(
            block_on(reversed.admit(&parts_with(&[
                ("authorization", "Bearer beta-token"),
                ("x-fake-user", "alice"),
            ])))
            .unwrap()
            .method,
            "fake"
        );
    }

    #[test]
    fn a_denial_names_every_way_in() {
        let c = bearer_and_fake();
        let d = block_on(c.admit(&parts(None))).unwrap_err();
        assert_eq!(d.error, AuthError::Missing);
        assert!(d.rejected_by.is_empty());
        assert_eq!(
            d.challenge.as_deref(),
            Some(r#"Bearer realm="pscat-mcp", Fake realm="test""#)
        );
        // Both reject: the first rejection is reported, each method's own
        // challenge says what it rejected.
        let d = block_on(c.admit(&parts_with(&[
            ("authorization", "Basic x"),
            ("x-fake-user", "mallory"),
        ])))
        .unwrap_err();
        assert_eq!(d.error, AuthError::Malformed);
        assert_eq!(d.rejected_by, ["bearer", "fake"]);
        assert_eq!(
            d.challenge.as_deref(),
            Some(r#"Bearer realm="pscat-mcp", Fake realm="test", error="invalid_token""#)
        );
        // Only the fake method rejects.
        let d = block_on(c.admit(&parts_with(&[("x-fake-user", "mallory")]))).unwrap_err();
        assert_eq!(d.error, AuthError::InvalidToken);
        assert_eq!(d.rejected_by, ["fake"]);
    }

    #[test]
    fn a_fallback_never_overrides_a_presented_credential() {
        // Whatever the order, a fallback only admits a request that no
        // other method found a credential in.
        for fallback_first in [true, false] {
            let fallback: Box<dyn AuthMethod> = Box::new(NoAuth::default());
            let c = if fallback_first {
                chain(vec![fallback, Box::new(auth())])
            } else {
                chain(vec![Box::new(auth()), fallback])
            };
            assert_eq!(
                block_on(c.admit(&parts(None))).unwrap(),
                Principal::anonymous(),
                "{fallback_first}"
            );
            assert_eq!(
                block_on(c.admit(&parts(Some("Bearer alpha-token")))).unwrap(),
                Principal::new("bearer", "token#0"),
                "{fallback_first}"
            );
            let d = block_on(c.admit(&parts(Some("Bearer wrong")))).unwrap_err();
            assert_eq!(d.error, AuthError::InvalidToken, "{fallback_first}");
            assert_eq!(d.rejected_by, ["bearer"]);
            let d = block_on(c.admit(&parts(Some("Basic x")))).unwrap_err();
            assert_eq!(d.error, AuthError::Malformed, "{fallback_first}");
        }
        // A fake fallback beside bearer, and a non-fallback fake: the
        // fallback flag, not the method, decides.
        let c = chain(vec![Box::new(Fake { fallback: true }), Box::new(auth())]);
        assert!(
            block_on(c.admit(&parts_with(&[
                ("authorization", "Bearer wrong"),
                ("x-fake-user", "alice"),
            ])))
            .is_err()
        );
        assert_eq!(
            block_on(c.admit(&parts_with(&[("x-fake-user", "alice")]))).unwrap(),
            Principal::new("fake", "fake:alice")
        );
    }

    #[test]
    fn challenges_merge_by_scheme() {
        let mut c = Challenges::default();
        assert_eq!(c.header_value(), None);
        c.add(Challenge::new("Bearer").param("realm", "pscat-mcp"));
        c.add(Challenge::new("bearer").param("realm", "other").param(
            "resource_metadata",
            "https://h.example/.well-known/oauth-protected-resource",
        ));
        c.add(Challenge::new("Basic"));
        c.add(Challenge::new("Fake").param("note", r#"a "quoted" \ value"#));
        assert_eq!(
            c.header_value().unwrap(),
            r#"Bearer realm="pscat-mcp", resource_metadata="https://h.example/.well-known/oauth-protected-resource", Basic, Fake note="a \"quoted\" \\ value""#
        );
        assert_eq!(c.0[0].scheme(), "Bearer");
    }

    #[test]
    fn every_error_is_a_401_today() {
        for e in [
            AuthError::Missing,
            AuthError::Malformed,
            AuthError::InvalidToken,
        ] {
            assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        }
    }
}
