//! Guard rails for the explicit no-auth mode (`PSCAT_MCP_AUTH=...,none`,
//! issue #120).
//!
//! The `none` method ([`crate::auth::NoAuth`]) admits a request that carries
//! no credential as the principal `anonymous`. Anyone who can reach the URL
//! can then spend render CPU time and quota, so it is for staging, and it
//! never turns on by itself. While it is on, [`NoneGuard`] checks every
//! request that reaches it, in this order, and answers with an
//! [`AuthError`] that carries the status:
//!
//! 1. **Expiry** (`PSCAT_MCP_AUTH_NONE_UNTIL`, RFC 3339): from that instant
//!    on, requests get `403 unauthenticated_access_expired`. Startup refuses
//!    a time that has already passed, but a Cloud Run instance can outlive
//!    it, so the time is checked on every request.
//! 2. **Source allowlist** (`PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS`): a client
//!    address outside every listed CIDR gets `403 source_not_allowed`. An
//!    address that cannot be determined is outside the list. Rejected
//!    sources never create rate-limit state.
//! 3. **Rate limit** (`PSCAT_MCP_AUTH_NONE_RATE_LIMIT` requests per
//!    `PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS`, per client address): over it,
//!    `429 rate_limited` with a `Retry-After`. A fixed window, so it is
//!    exact and does not depend on timing inside the window. It is on top of
//!    the render concurrency cap and the unchanged resource limits.
//!
//! Only requests that reach `none` are counted: `none` is a fallback, so a
//! request with a valid or invalid bearer token never gets here.
//!
//! # The client address
//!
//! The address a request is attributed to is *not* read blindly from
//! `X-Forwarded-For`: a client sets that header itself, and a proxy only
//! appends to it. By default the TCP peer address is used. With
//! `PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS=N` (`N` trusted proxies in front of the
//! server, each appending the address it saw), the address is the `N`-th
//! entry from the right of the combined `X-Forwarded-For` header. Entries to
//! the left of it are client-controlled and never used. With fewer than `N`
//! entries, or an entry that is not an IP address, the address is unknown,
//! which the guard treats as outside the allowlist (fail closed). IPv4-mapped
//! IPv6 addresses count as the IPv4 address, and IPv6 clients are limited
//! per `/64`.
//!
//! Rejections are logged with their `reason` only; the client address is
//! logged at DEBUG at most, never at INFO.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::ConnectInfo;
use chrono::{DateTime, Duration, Utc};
use clap::Args;

use crate::auth::AuthError;

/// The most client addresses the rate limiter tracks at once. When the table
/// is full of live windows, new clients are limited until one expires, so a
/// sprayed address space cannot grow memory without bound.
const MAX_TRACKED_CLIENTS: usize = 100_000;

/// Default of `--auth-none-rate-limit`.
pub const DEFAULT_RATE_LIMIT: u32 = 60;
/// Default of `--auth-none-rate-window-secs`.
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 60;

/// The no-auth mode's own settings. Only read when `none` is in
/// `PSCAT_MCP_AUTH`; set without it they are ignored, with a warning.
#[derive(Debug, Clone, Args)]
pub struct NoneAuthConfig {
    /// With `none` in `--auth`: the time, RFC 3339 (for example
    /// `2026-12-01T00:00:00Z`), after which no-auth requests are refused
    /// with 403. The server refuses to start if it has already passed.
    #[arg(
        long = "auth-none-until",
        env = "PSCAT_MCP_AUTH_NONE_UNTIL",
        value_name = "RFC3339"
    )]
    pub until: Option<String>,

    /// With `none` in `--auth`: source CIDRs allowed to use it, comma
    /// separated (`203.0.113.0/24,2001:db8::/32`; a bare address is one
    /// host). Empty: any source. The client address is the TCP peer, or
    /// follows `--auth-none-trusted-hops`.
    #[arg(
        long = "auth-none-allow-cidrs",
        env = "PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS",
        value_delimiter = ',',
        value_name = "CIDRS"
    )]
    pub allow_cidrs: Vec<String>,

    /// With `none` in `--auth`: requests one client address may make per
    /// rate window, before `429`. At least 1.
    #[arg(
        long = "auth-none-rate-limit",
        env = "PSCAT_MCP_AUTH_NONE_RATE_LIMIT",
        default_value_t = DEFAULT_RATE_LIMIT
    )]
    pub rate_limit: u32,

    /// With `none` in `--auth`: the length of the rate window, in seconds.
    /// At least 1.
    #[arg(
        long = "auth-none-rate-window-secs",
        env = "PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS",
        default_value_t = DEFAULT_RATE_WINDOW_SECS
    )]
    pub rate_window_secs: u64,

    /// With `none` in `--auth`: how many trusted proxies sit in front of
    /// the server, each appending the address it saw to `X-Forwarded-For`.
    /// 0 (the default): the TCP peer address is the client. N: the N-th
    /// entry from the right of `X-Forwarded-For`. Cloud Run: see
    /// `docs/CLOUD_RUN.md`.
    #[arg(
        long = "auth-none-trusted-hops",
        env = "PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS",
        default_value_t = 0
    )]
    pub trusted_hops: usize,
}

impl Default for NoneAuthConfig {
    fn default() -> Self {
        NoneAuthConfig {
            until: None,
            allow_cidrs: Vec::new(),
            rate_limit: DEFAULT_RATE_LIMIT,
            rate_window_secs: DEFAULT_RATE_WINDOW_SECS,
            trusted_hops: 0,
        }
    }
}

impl NoneAuthConfig {
    /// Whether any setting differs from its default (used to warn when the
    /// settings are given but `none` is not enabled).
    pub fn is_customised(&self) -> bool {
        let d = NoneAuthConfig::default();
        self.until.is_some()
            || self.allow_cidrs.iter().any(|c| !c.trim().is_empty())
            || self.rate_limit != d.rate_limit
            || self.rate_window_secs != d.rate_window_secs
            || self.trusted_hops != d.trusted_hops
    }
}

/// The time source: injected so that expiry and rate windows are testable.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// A source network: an address and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/n`, `x::/n` or a bare address (a single host).
    /// Host bits below the prefix are ignored.
    pub fn parse(text: &str) -> Result<Cidr, String> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a.trim(), Some(p.trim())),
            None => (text, None),
        };
        let net: IpAddr = addr
            .parse()
            .map_err(|_| format!("{text:?} is not an IP address or CIDR"))?;
        // An IPv4-mapped IPv6 network (`::ffff:a.b.c.d/N`, N >= 96) is the
        // IPv4 network with a prefix 96 shorter.
        let mapped = net.is_ipv6() && net.to_canonical().is_ipv4();
        let net = net.to_canonical();
        let (max, skip) = match (net.is_ipv4(), mapped) {
            (false, _) => (128, 0),
            (true, true) => (128, 96),
            (true, false) => (32, 0),
        };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max && *p >= skip)
                .ok_or_else(|| format!("{text:?} has a prefix length outside {skip}..={max}"))?,
        } - skip;
        Ok(Cidr {
            net: mask(net, prefix),
            prefix,
        })
    }

    /// Whether `ip` (an IPv4-mapped address counts as its IPv4 address) is
    /// inside the network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.net.is_ipv4() && mask(ip, self.prefix) == self.net
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let m = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((bits & m).into())
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let m = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6((bits & m).into())
        }
    }
}

/// One entry of `X-Forwarded-For`: an address, optionally with a port or in
/// brackets (`203.0.113.7`, `203.0.113.7:4711`, `[2001:db8::1]:4711`).
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(sock) = entry.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    entry
        .strip_prefix('[')
        .and_then(|e| e.strip_suffix(']'))
        .and_then(|e| e.parse::<Ipv6Addr>().ok())
        .map(IpAddr::V6)
}

/// The address `parts` is attributed to, or `None` when it cannot be
/// determined. See the module docs: the TCP peer when `trusted_hops` is 0,
/// otherwise the `trusted_hops`-th entry from the right of the combined
/// `X-Forwarded-For` header.
pub fn client_address(parts: &http::request::Parts, trusted_hops: usize) -> Option<IpAddr> {
    let ip = if trusted_hops == 0 {
        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())?
    } else {
        let entries: Vec<&str> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .collect();
        let entry = entries.len().checked_sub(trusted_hops)?;
        parse_forwarded_entry(entries[entry])?
    };
    Some(ip.to_canonical())
}

/// The rate-limit key of `ip`: the address itself, or the `/64` of an IPv6
/// address (a client usually holds a whole `/64`).
fn limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(_) => mask(ip, 64),
        v4 => v4,
    }
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: DateTime<Utc>,
    count: u32,
}

/// The guard rails of the `none` method. See the module docs.
pub struct NoneGuard {
    until: Option<DateTime<Utc>>,
    allow: Vec<Cidr>,
    limit: u32,
    window: Duration,
    trusted_hops: usize,
    clock: Clock,
    windows: Mutex<HashMap<IpAddr, Window>>,
}

impl std::fmt::Debug for NoneGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoneGuard")
            .field("summary", &self.summary())
            .finish()
    }
}

impl NoneGuard {
    /// Build the guard from `config` with the system clock. Fails with a
    /// message for an unparsable or already passed `until`, a bad CIDR, or a
    /// rate limit or window of 0.
    pub fn new(config: &NoneAuthConfig) -> Result<NoneGuard, String> {
        NoneGuard::with_clock(config, Arc::new(Utc::now))
    }

    /// [`NoneGuard::new`] with an injected clock.
    pub fn with_clock(config: &NoneAuthConfig, clock: Clock) -> Result<NoneGuard, String> {
        let until = match config.until.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(text) => {
                let until = DateTime::parse_from_rfc3339(text)
                    .map_err(|e| {
                        format!(
                            "PSCAT_MCP_AUTH_NONE_UNTIL / --auth-none-until {text:?} is not an \
                             RFC 3339 time such as 2026-12-01T00:00:00Z: {e}"
                        )
                    })?
                    .with_timezone(&Utc);
                if clock() >= until {
                    return Err(format!(
                        "auth method `none` has expired: PSCAT_MCP_AUTH_NONE_UNTIL / \
                         --auth-none-until is {}, which has already passed; set a later time, \
                         or remove `none` from PSCAT_MCP_AUTH",
                        until.to_rfc3339()
                    ));
                }
                Some(until)
            }
        };
        let mut allow = Vec::new();
        for item in config
            .allow_cidrs
            .iter()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
        {
            allow.push(Cidr::parse(item).map_err(|e| {
                format!("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS / --auth-none-allow-cidrs: {e}")
            })?);
        }
        if config.rate_limit == 0 {
            return Err(
                "PSCAT_MCP_AUTH_NONE_RATE_LIMIT / --auth-none-rate-limit must be at least 1".into(),
            );
        }
        let window_secs = i64::try_from(config.rate_window_secs)
            .ok()
            .filter(|s| *s >= 1 && *s <= 366 * 24 * 3600)
            .ok_or_else(|| {
                "PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS / --auth-none-rate-window-secs must be \
                 between 1 and 31622400"
                    .to_string()
            })?;
        Ok(NoneGuard {
            until,
            allow,
            limit: config.rate_limit,
            window: Duration::seconds(window_secs),
            trusted_hops: config.trusted_hops,
            clock,
            windows: Mutex::new(HashMap::new()),
        })
    }

    /// A one-line description for the startup log. Names no client.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "none (UNAUTHENTICATED: {} requests per {}s per client address",
            self.limit,
            self.window.num_seconds()
        );
        if self.allow.is_empty() {
            s.push_str(", any source");
        } else {
            s.push_str(&format!(", {} allowed CIDR(s)", self.allow.len()));
        }
        s.push_str(&format!(", {} trusted proxy hop(s)", self.trusted_hops));
        if let Some(until) = self.until {
            s.push_str(&format!(", until {}", until.to_rfc3339()));
        }
        s.push(')');
        s
    }

    /// When the mode expires, if it does.
    pub fn until(&self) -> Option<DateTime<Utc>> {
        self.until
    }

    /// Admit or refuse one request that reached `none`. See the module docs
    /// for the order of the checks.
    pub fn check(&self, parts: &http::request::Parts) -> Result<(), AuthError> {
        let now = (self.clock)();
        if self.until.is_some_and(|until| now >= until) {
            return Err(AuthError::AccessExpired);
        }
        let Some(ip) = client_address(parts, self.trusted_hops) else {
            tracing::debug!("none: the client address cannot be determined");
            return Err(AuthError::SourceNotAllowed);
        };
        tracing::debug!(client = %ip, "none: client address");
        if !self.allow.is_empty() && !self.allow.iter().any(|c| c.contains(ip)) {
            return Err(AuthError::SourceNotAllowed);
        }
        self.take(limit_key(ip), now)
    }

    /// Count one request against `key`'s window at `now`.
    fn take(&self, key: IpAddr, now: DateTime<Utc>) -> Result<(), AuthError> {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let live = |w: &Window| now >= w.start && now < w.start + self.window;
        if !windows.contains_key(&key) && windows.len() >= MAX_TRACKED_CLIENTS {
            windows.retain(|_, w| live(w));
            if windows.len() >= MAX_TRACKED_CLIENTS {
                return Err(self.limited(now, now));
            }
        }
        let w = windows.entry(key).or_insert(Window {
            start: now,
            count: 0,
        });
        if !live(w) {
            *w = Window {
                start: now,
                count: 0,
            };
        }
        if w.count >= self.limit {
            return Err(self.limited(w.start, now));
        }
        w.count += 1;
        Ok(())
    }

    /// `RateLimited`, retrying when the window that began at `start` ends.
    fn limited(&self, start: DateTime<Utc>, now: DateTime<Utc>) -> AuthError {
        let left = (start + self.window - now).num_milliseconds().max(0) as u64;
        AuthError::RateLimited {
            retry_after_secs: left.div_ceil(1000).max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    const T0: i64 = 1_800_000_000_000; // milliseconds since the epoch

    /// A clock tests move by hand.
    fn clock() -> (Clock, Arc<AtomicI64>) {
        let ms = Arc::new(AtomicI64::new(T0));
        let c = ms.clone();
        (
            Arc::new(move || DateTime::from_timestamp_millis(c.load(Ordering::SeqCst)).unwrap()),
            ms,
        )
    }

    fn parts(peer: Option<&str>, xff: &[&str]) -> http::request::Parts {
        let mut b = http::Request::builder().uri("/mcp");
        for v in xff {
            b = b.header("x-forwarded-for", *v);
        }
        let mut parts = b.body(()).unwrap().into_parts().0;
        if let Some(p) = peer {
            parts.extensions.insert(ConnectInfo(SocketAddr::new(
                p.parse::<IpAddr>().unwrap(),
                5555,
            )));
        }
        parts
    }

    fn peer(ip: &str) -> http::request::Parts {
        parts(Some(ip), &[])
    }

    fn config() -> NoneAuthConfig {
        NoneAuthConfig::default()
    }

    #[test]
    fn cidr_parsing_and_matching() {
        let c = Cidr::parse("203.0.113.0/24").unwrap();
        assert!(c.contains("203.0.113.7".parse().unwrap()));
        assert!(!c.contains("203.0.114.7".parse().unwrap()));
        // Host bits are ignored, a bare address is one host.
        assert!(
            Cidr::parse("10.1.2.3/8")
                .unwrap()
                .contains("10.9.9.9".parse().unwrap())
        );
        let host = Cidr::parse("192.0.2.1").unwrap();
        assert!(host.contains("192.0.2.1".parse().unwrap()));
        assert!(!host.contains("192.0.2.2".parse().unwrap()));
        // /0 is everything of its family and only that.
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(
            !Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("2001:db8::1".parse().unwrap())
        );
        // IPv6, and an IPv4-mapped address is its IPv4 address.
        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::7".parse().unwrap()));
        assert!(!v6.contains("2001:db9::7".parse().unwrap()));
        assert!(c.contains("::ffff:203.0.113.7".parse().unwrap()));
        assert!(
            Cidr::parse("::ffff:203.0.113.0/120")
                .unwrap()
                .contains("203.0.113.9".parse().unwrap())
        );
        for bad in [
            "",
            "nope",
            "10.0.0.0/33",
            "10.0.0.0/x",
            "::1/129",
            "10.0.0/8",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn client_address_without_trusted_hops_is_the_peer() {
        // A forged header changes nothing.
        let p = parts(Some("198.51.100.4"), &["10.0.0.1, 10.0.0.2"]);
        assert_eq!(client_address(&p, 0), Some("198.51.100.4".parse().unwrap()));
        // No connection info: unknown.
        assert_eq!(client_address(&parts(None, &["10.0.0.1"]), 0), None);
        // A mapped peer is its IPv4 address.
        assert_eq!(
            client_address(&peer("::ffff:198.51.100.4"), 0),
            Some("198.51.100.4".parse().unwrap())
        );
    }

    #[test]
    fn client_address_takes_the_nth_entry_from_the_right() {
        let p = parts(Some("127.0.0.1"), &["1.1.1.1, 2.2.2.2, 3.3.3.3"]);
        assert_eq!(client_address(&p, 1), Some("3.3.3.3".parse().unwrap()));
        assert_eq!(client_address(&p, 2), Some("2.2.2.2".parse().unwrap()));
        assert_eq!(client_address(&p, 3), Some("1.1.1.1".parse().unwrap()));
        // Fewer entries than hops: unknown, never the leftmost.
        assert_eq!(client_address(&p, 4), None);
        // Several header lines are one list, in order.
        let p = parts(Some("127.0.0.1"), &["1.1.1.1", "2.2.2.2, 3.3.3.3"]);
        assert_eq!(client_address(&p, 2), Some("2.2.2.2".parse().unwrap()));
        // No header at all, and an entry that is not an address.
        assert_eq!(client_address(&peer("127.0.0.1"), 1), None);
        assert_eq!(
            client_address(&parts(Some("127.0.0.1"), &["1.1.1.1, unknown"]), 1),
            None
        );
        assert_eq!(
            client_address(&parts(Some("127.0.0.1"), &["1.1.1.1,"]), 1),
            None
        );
        // A port or brackets are tolerated; an IPv4-mapped entry is IPv4.
        let p = parts(Some("127.0.0.1"), &["x, 3.3.3.3:4711"]);
        assert_eq!(client_address(&p, 1), Some("3.3.3.3".parse().unwrap()));
        let p = parts(Some("127.0.0.1"), &["[2001:db8::1]:4711"]);
        assert_eq!(client_address(&p, 1), Some("2001:db8::1".parse().unwrap()));
        let p = parts(Some("127.0.0.1"), &["::ffff:3.3.3.3"]);
        assert_eq!(client_address(&p, 1), Some("3.3.3.3".parse().unwrap()));
    }

    #[test]
    fn a_forged_header_cannot_escape_the_allowlist() {
        let (c, _) = clock();
        let cfg = NoneAuthConfig {
            allow_cidrs: vec!["10.0.0.0/8".into()],
            ..config()
        };
        // No trusted proxy: the header is ignored, the peer decides.
        let g = NoneGuard::with_clock(&cfg, c.clone()).unwrap();
        assert_eq!(
            g.check(&parts(Some("198.51.100.4"), &["10.1.1.1"])),
            Err(AuthError::SourceNotAllowed)
        );
        assert_eq!(g.check(&parts(Some("10.2.2.2"), &["198.51.100.4"])), Ok(()));
        // One trusted proxy: it appended the real address after the forged one.
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                trusted_hops: 1,
                ..cfg.clone()
            },
            c,
        )
        .unwrap();
        assert_eq!(
            g.check(&parts(Some("127.0.0.1"), &["10.1.1.1, 198.51.100.4"])),
            Err(AuthError::SourceNotAllowed)
        );
        assert_eq!(
            g.check(&parts(Some("127.0.0.1"), &["198.51.100.4, 10.1.1.1"])),
            Ok(())
        );
        // Unknown: refused.
        assert_eq!(
            g.check(&parts(Some("10.1.1.1"), &[])),
            Err(AuthError::SourceNotAllowed)
        );
    }

    #[test]
    fn the_rate_limit_is_per_client_and_resets_with_the_window() {
        let (c, ms) = clock();
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                rate_limit: 2,
                rate_window_secs: 60,
                ..config()
            },
            c,
        )
        .unwrap();
        let a = peer("198.51.100.4");
        let b = peer("198.51.100.5");
        assert_eq!(g.check(&a), Ok(()));
        ms.fetch_add(10_000, Ordering::SeqCst);
        assert_eq!(g.check(&a), Ok(()));
        ms.fetch_add(10_000, Ordering::SeqCst);
        // 20 s into the window: 40 s to wait.
        assert_eq!(
            g.check(&a),
            Err(AuthError::RateLimited {
                retry_after_secs: 40
            })
        );
        // Another client has its own budget.
        assert_eq!(g.check(&b), Ok(()));
        // A refused request does not extend the window.
        ms.fetch_add(39_001, Ordering::SeqCst);
        assert_eq!(
            g.check(&a),
            Err(AuthError::RateLimited {
                retry_after_secs: 1
            })
        );
        ms.fetch_add(1_000, Ordering::SeqCst);
        assert_eq!(g.check(&a), Ok(()));
    }

    #[test]
    fn forged_forwarded_for_neither_resets_nor_borrows_a_limit() {
        let (c, _) = clock();
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                rate_limit: 2,
                trusted_hops: 1,
                ..config()
            },
            c.clone(),
        )
        .unwrap();
        let from =
            |forged: &str, real: &str| parts(Some("127.0.0.1"), &[&format!("{forged}, {real}")]);
        assert_eq!(g.check(&from("1.1.1.1", "198.51.100.4")), Ok(()));
        assert_eq!(g.check(&from("2.2.2.2", "198.51.100.4")), Ok(()));
        // Changing the forged part does not give a fresh budget.
        for forged in ["3.3.3.3", "198.51.100.9", "unknown", ""] {
            assert!(matches!(
                g.check(&from(forged, "198.51.100.4")),
                Err(AuthError::RateLimited { .. })
            ));
        }
        // And naming the victim does not spend the victim's budget.
        assert_eq!(g.check(&from("198.51.100.4", "198.51.100.9")), Ok(()));
        assert_eq!(g.check(&from("198.51.100.4", "198.51.100.9")), Ok(()));
        assert_eq!(g.check(&from("198.51.100.4", "198.51.100.10")), Ok(()));
        // Without a trusted proxy the header is not read at all: every
        // request is the peer.
        let plain = NoneGuard::with_clock(
            &NoneAuthConfig {
                rate_limit: 1,
                ..config()
            },
            c,
        )
        .unwrap();
        assert_eq!(
            plain.check(&parts(Some("198.51.100.4"), &["1.1.1.1"])),
            Ok(())
        );
        assert!(matches!(
            plain.check(&parts(Some("198.51.100.4"), &["2.2.2.2"])),
            Err(AuthError::RateLimited { .. })
        ));
    }

    #[test]
    fn rejected_sources_do_not_spend_or_create_limiter_state() {
        let (c, _) = clock();
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                allow_cidrs: vec!["10.0.0.0/8".into()],
                rate_limit: 1,
                ..config()
            },
            c,
        )
        .unwrap();
        for _ in 0..5 {
            assert_eq!(
                g.check(&peer("198.51.100.4")),
                Err(AuthError::SourceNotAllowed)
            );
        }
        assert!(g.windows.lock().unwrap().is_empty());
        assert_eq!(g.check(&peer("10.0.0.1")), Ok(()));
    }

    #[test]
    fn ipv6_clients_share_a_slash_64_and_mapped_addresses_are_ipv4() {
        let (c, _) = clock();
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                rate_limit: 1,
                ..config()
            },
            c,
        )
        .unwrap();
        assert_eq!(g.check(&peer("2001:db8:0:1::1")), Ok(()));
        assert!(g.check(&peer("2001:db8:0:1:ffff::2")).is_err());
        assert_eq!(g.check(&peer("2001:db8:0:2::1")), Ok(()));
        assert_eq!(g.check(&peer("198.51.100.4")), Ok(()));
        assert!(g.check(&peer("::ffff:198.51.100.4")).is_err());
    }

    #[test]
    fn the_table_is_bounded() {
        let (c, ms) = clock();
        let g = NoneGuard::with_clock(&config(), c).unwrap();
        let now = (g.clock)();
        {
            let mut w = g.windows.lock().unwrap();
            for i in 0..MAX_TRACKED_CLIENTS as u32 {
                w.insert(
                    IpAddr::V4(std::net::Ipv4Addr::from(i + 0x0100_0000)),
                    Window {
                        start: now,
                        count: 1,
                    },
                );
            }
        }
        // Full of live windows: a new client is limited until one expires.
        assert!(matches!(
            g.check(&peer("198.51.100.4")),
            Err(AuthError::RateLimited { .. })
        ));
        // A known client still gets its own window's answer.
        assert_eq!(g.check(&peer("1.0.0.0")), Ok(()));
        ms.fetch_add(61_000, Ordering::SeqCst);
        assert_eq!(g.check(&peer("198.51.100.4")), Ok(()));
        assert!(g.windows.lock().unwrap().len() < MAX_TRACKED_CLIENTS);
    }

    #[test]
    fn expiry_is_checked_at_startup_and_on_every_request() {
        let (c, ms) = clock();
        let until = DateTime::from_timestamp_millis(T0 + 5_000).unwrap();
        let cfg = NoneAuthConfig {
            until: Some(until.to_rfc3339()),
            ..config()
        };
        let g = NoneGuard::with_clock(&cfg, c.clone()).unwrap();
        assert_eq!(g.until(), Some(until));
        assert_eq!(g.check(&peer("198.51.100.4")), Ok(()));
        ms.fetch_add(4_999, Ordering::SeqCst);
        assert_eq!(g.check(&peer("198.51.100.4")), Ok(()));
        ms.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            g.check(&peer("198.51.100.4")),
            Err(AuthError::AccessExpired)
        );
        // Expiry wins over everything else, and a restart refuses.
        assert_eq!(g.check(&peer("10.0.0.1")), Err(AuthError::AccessExpired));
        let err = NoneGuard::with_clock(&cfg, c).err().unwrap();
        assert!(
            err.contains("has expired") && err.contains("already passed"),
            "{err}"
        );
    }

    #[test]
    fn bad_settings_are_refused_with_a_message() {
        let (c, _) = clock();
        let refused = |cfg: NoneAuthConfig| NoneGuard::with_clock(&cfg, c.clone()).err().unwrap();
        let e = refused(NoneAuthConfig {
            until: Some("tomorrow".into()),
            ..config()
        });
        assert!(e.contains("not an RFC 3339 time"), "{e}");
        let e = refused(NoneAuthConfig {
            allow_cidrs: vec!["10.0.0.0/8".into(), "nope".into()],
            ..config()
        });
        assert!(
            e.contains("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS") && e.contains("nope"),
            "{e}"
        );
        assert!(
            refused(NoneAuthConfig {
                rate_limit: 0,
                ..config()
            })
            .contains("RATE_LIMIT")
        );
        assert!(
            refused(NoneAuthConfig {
                rate_window_secs: 0,
                ..config()
            })
            .contains("WINDOW_SECS")
        );
        // Empty items and blank values are skipped.
        let ok = NoneAuthConfig {
            until: Some(" ".into()),
            allow_cidrs: vec![String::new(), " ".into()],
            ..config()
        };
        let g = NoneGuard::with_clock(&ok, c).unwrap();
        assert_eq!(g.check(&peer("198.51.100.4")), Ok(()));
    }

    #[test]
    fn the_summary_names_no_client() {
        let (c, _) = clock();
        let g = NoneGuard::with_clock(
            &NoneAuthConfig {
                allow_cidrs: vec!["203.0.113.0/24".into()],
                trusted_hops: 1,
                ..config()
            },
            c,
        )
        .unwrap();
        let s = g.summary();
        assert!(
            s.contains("UNAUTHENTICATED") && s.contains("1 allowed CIDR(s)"),
            "{s}"
        );
        assert!(!s.contains("203.0.113"), "{s}");
    }
}
