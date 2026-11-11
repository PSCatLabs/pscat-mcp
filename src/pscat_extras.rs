//! The one place this crate calls PSCat APIs that are `#[doc(hidden)]`.
//!
//! PSCat exports these modules only so its own binaries and tests can reach
//! them; they are not part of its documented API, so they may change in any
//! release. Wrapping every use here means a PSCat bump that breaks one is
//! fixed in this file alone. Tracking issue for a supported API:
//! <https://github.com/PSCatLabs/pscat/issues/1098>.
//!
//! Hidden APIs in use:
//!
//! * `pscat::lexer` ([`syntax_scan`]), the tokenizer pass of
//!   `validate_postscript` (PSCatLabs/pscat#1096).
//! * `pscat::gfx::PageSizeSource` ([`allow_program_page_size`]), so a
//!   program's `setpagedevice` may replace a default-sized page
//!   (PSCatLabs/pscat#1097).
//! * `pscat::capabilities` ([`capabilities_json`]), the catalog behind
//!   `pscat --capabilities` and the plugin's `describe_art_capabilities`.
//!
//! Nothing outside this module may name a `#[doc(hidden)]` PSCat item.
//! Everything here runs inside the worker process.

use serde_json::Value;

use crate::worker::SyntaxScan;

/// Scan `source` with PSCat's tokenizer without executing anything, and
/// check that procedure braces balance (the tokenizer reports `{`/`}` as
/// separate tokens and leaves balancing to the interpreter).
pub fn syntax_scan(source: &[u8]) -> SyntaxScan {
    let mut lexer = pscat::lexer::Lexer::new(source.to_vec());
    let mut tokens = 0usize;
    let mut depth = 0usize;
    loop {
        match lexer.next_token() {
            Ok(Some(token)) => {
                tokens += 1;
                match token {
                    pscat::lexer::Token::LBrace => depth += 1,
                    pscat::lexer::Token::RBrace => {
                        if depth == 0 {
                            return SyntaxScan {
                                ok: false,
                                tokens,
                                error: Some(format!("unmatched '}}' (token {tokens})")),
                            };
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                let error = (depth > 0).then(|| {
                    format!("unterminated procedure: {depth} '{{' without a matching '}}'")
                });
                return SyntaxScan {
                    ok: error.is_none(),
                    tokens,
                    error,
                };
            }
            Err(e) => {
                return SyntaxScan {
                    ok: false,
                    tokens,
                    error: Some(e.to_string()),
                };
            }
        }
    }
}

/// Mark `interp`'s page size as a default that nothing chose, like
/// `Interp::new()` does, so the program's `setpagedevice` may replace it
/// (bounded by `max_page_pixels`).
pub fn allow_program_page_size(interp: &mut pscat::Interp) {
    interp
        .gfx_mut()
        .set_page_size_source(pscat::gfx::PageSizeSource::Default);
}

/// The art catalog of the linked PSCat build, as `pscat --capabilities`
/// prints it: `pscat_version`, `catalog_signature`, `font_packs` and a
/// `capabilities` array (fonts, Type 3 faces, palettes, page templates,
/// artkit and style-pack procedures, native operators).
///
/// Fonts are listed by scanning font directories on disk, so call this in
/// the process whose environment renders use (the worker). Absolute
/// filesystem paths in the font-pack status are removed, so the reply does
/// not disclose where the server keeps its files.
pub fn capabilities_json() -> Value {
    let mut payload = pscat::capabilities::payload_json();
    scrub_paths(&mut payload);
    payload
}

/// Remove every `path` key from `font_packs.installed[]` and
/// `font_packs.overrides[]`; those are the only host paths in the catalog.
fn scrub_paths(payload: &mut Value) {
    let Some(packs) = payload.get_mut("font_packs") else {
        return;
    };
    for section in ["installed", "overrides"] {
        if let Some(Value::Array(items)) = packs.get_mut(section) {
            for item in items {
                if let Value::Object(map) = item {
                    map.remove("path");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_removes_only_paths() {
        let mut v = json!({
            "font_packs": {
                "installed": [{"name": "a", "path": "/srv/x", "status": "pinned"}],
                "overrides": [{"path": "/srv/y", "status": "user-selected"}],
                "available": ["a"]
            },
            "capabilities": []
        });
        scrub_paths(&mut v);
        assert_eq!(
            v,
            json!({
                "font_packs": {
                    "installed": [{"name": "a", "status": "pinned"}],
                    "overrides": [{"status": "user-selected"}],
                    "available": ["a"]
                },
                "capabilities": []
            })
        );
    }

    #[test]
    fn syntax_scan_checks_braces() {
        assert!(syntax_scan(b"{ 1 2 add } exec").ok);
        assert!(!syntax_scan(b"{ 1").ok);
        assert!(!syntax_scan(b"1 }").ok);
    }
}
