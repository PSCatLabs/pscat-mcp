//! Views of PSCat's art catalog for `describe_art_capabilities`.
//!
//! The whole catalog is about 280 KB, far more than a model should read in
//! one reply, so a call returns one of three bounded views and each reply
//! stays under [`REPLY_BUDGET_BYTES`] in both its structured and its text
//! form:
//!
//! * **index** (no arguments): every kind with the names under it;
//! * **kind**: the names of one kind with a one-line description each, a page
//!   of at most [`MAX_PAGE`] at a time;
//! * **names**: the full entries (parameters, load line, example) of up to
//!   [`MAX_NAMES`] names, as many as fit in [`ENTRIES_BUDGET_BYTES`]; the
//!   rest are listed as `omitted` to ask for again.
//!
//! [`CatalogSlice`] is the structured form; [`CatalogSlice::text`] renders
//! the same information for a client that shows only `content`.

use serde::Serialize;

use crate::render::{ArtCapability, ArtCatalog, ErrorCode, FontPacks, RenderError};

/// The kinds a capability can have, as `pscat --capabilities` names them.
pub const KINDS: [&str; 7] = [
    "font",
    "type3_face",
    "palette",
    "template",
    "procedure",
    "operator",
    "dial",
];

/// Most names one call may ask for.
pub const MAX_NAMES: usize = 20;
/// Entries in a `kind` page when `limit` is not given.
pub const DEFAULT_PAGE: usize = 100;
/// Most entries in a `kind` page.
pub const MAX_PAGE: usize = 100;
/// Characters of a description kept in a `kind` listing.
pub const DESCRIPTION_CHARS: usize = 100;
/// Most bytes of serialized full entries in one `names` reply.
pub const ENTRIES_BUDGET_BYTES: usize = 16 * 1024;
/// What every reply stays under, structured and text alike.
pub const REPLY_BUDGET_BYTES: usize = 24 * 1024;

/// What a call asks for.
#[derive(Debug, Clone, Default)]
pub struct CatalogQuery {
    /// Restrict to one kind.
    pub kind: Option<String>,
    /// Exact names to return in full.
    pub names: Option<Vec<String>>,
    /// First entry of a `kind` page.
    pub offset: Option<u32>,
    /// Entries in a `kind` page.
    pub limit: Option<u32>,
}

fn invalid(message: impl Into<String>) -> RenderError {
    RenderError::new(ErrorCode::InvalidRequest, message)
}

impl CatalogQuery {
    /// Check the arguments without the catalog, so a bad call never starts
    /// a worker.
    pub fn check(&self) -> Result<(), RenderError> {
        if let Some(kind) = &self.kind
            && !KINDS.contains(&kind.as_str())
        {
            return Err(invalid(format!(
                "kind {kind:?} is not one of {}",
                KINDS.join(", ")
            )));
        }
        if let Some(names) = &self.names {
            if names.is_empty() {
                return Err(invalid("names is empty; give at least one name"));
            }
            if names.len() > MAX_NAMES {
                return Err(invalid(format!(
                    "names has {} entries; at most {MAX_NAMES} are accepted",
                    names.len()
                )));
            }
        }
        if (self.offset.is_some() || self.limit.is_some())
            && (self.kind.is_none() || self.names.is_some())
        {
            return Err(invalid(
                "offset and limit page through a kind listing; give kind and no names",
            ));
        }
        if let Some(limit) = self.limit
            && !(1..=MAX_PAGE as u32).contains(&limit)
        {
            return Err(invalid(format!("limit must be between 1 and {MAX_PAGE}")));
        }
        Ok(())
    }
}

/// One kind in the index.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct KindIndex {
    /// The kind.
    pub kind: String,
    /// Capabilities of this kind.
    pub count: usize,
    /// Their names, in catalog order.
    pub names: Vec<String>,
}

/// A name with a one-line description.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Summary {
    /// Name as a program uses it.
    pub name: String,
    /// The first line of the description, cut to a fixed length.
    pub description: String,
}

/// The part of the catalog one call returns.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CatalogSlice {
    /// `index`, `kind` or `names`.
    pub view: String,
    /// PSCat version that produced the catalog.
    pub pscat_version: String,
    /// Fingerprint of which capabilities are available. A change here or in
    /// `pscat_version` means the set changed.
    pub catalog_signature: String,
    /// Installed font packs (index view). Their directories are not reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_packs: Option<FontPacks>,
    /// Every kind with its names (index view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<Vec<KindIndex>>,
    /// The kind listed (kind view), or the filter applied (names view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Capabilities of that kind (kind view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Index of the first listed entry (kind view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// `offset` of the next page, when there is one (kind view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// The page of names with descriptions (kind view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summaries: Option<Vec<Summary>>,
    /// Full entries, in the order the names were asked for (names view). A
    /// name that exists under several kinds returns one entry per kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<ArtCapability>>,
    /// Asked-for names the catalog does not have (names view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_found: Option<Vec<String>>,
    /// Asked-for names that exist but did not fit in the reply; ask for them
    /// again (names view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Vec<String>>,
}

fn base(catalog: &ArtCatalog, view: &str) -> CatalogSlice {
    CatalogSlice {
        view: view.to_string(),
        pscat_version: catalog.pscat_version.clone(),
        catalog_signature: catalog.catalog_signature.clone(),
        font_packs: None,
        index: None,
        kind: None,
        total: None,
        offset: None,
        next_offset: None,
        summaries: None,
        entries: None,
        not_found: None,
        omitted: None,
    }
}

/// The first line of `text`, cut to [`DESCRIPTION_CHARS`] characters.
fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line.to_string(),
    }
}

/// The view of `catalog` that `query` asks for.
pub fn slice(catalog: &ArtCatalog, query: &CatalogQuery) -> Result<CatalogSlice, RenderError> {
    query.check()?;
    Ok(match (&query.names, &query.kind) {
        (Some(names), kind) => names_view(catalog, names, kind.as_deref()),
        (None, Some(kind)) => kind_view(catalog, kind, query),
        (None, None) => index_view(catalog),
    })
}

fn index_view(catalog: &ArtCatalog) -> CatalogSlice {
    let index = KINDS
        .iter()
        .filter_map(|kind| {
            let names: Vec<String> = catalog
                .capabilities
                .iter()
                .filter(|c| c.kind == *kind)
                .map(|c| c.name.clone())
                .collect();
            (!names.is_empty()).then(|| KindIndex {
                kind: kind.to_string(),
                count: names.len(),
                names,
            })
        })
        .collect();
    CatalogSlice {
        font_packs: Some(catalog.font_packs.clone()),
        index: Some(index),
        ..base(catalog, "index")
    }
}

fn kind_view(catalog: &ArtCatalog, kind: &str, query: &CatalogQuery) -> CatalogSlice {
    let all: Vec<&ArtCapability> = catalog
        .capabilities
        .iter()
        .filter(|c| c.kind == kind)
        .collect();
    let offset = query.offset.unwrap_or(0) as usize;
    let limit = query.limit.map_or(DEFAULT_PAGE, |l| l as usize);
    let page: Vec<Summary> = all
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| Summary {
            name: c.name.clone(),
            description: one_line(&c.description),
        })
        .collect();
    let end = offset.saturating_add(page.len());
    CatalogSlice {
        kind: Some(kind.to_string()),
        total: Some(all.len()),
        offset: Some(offset),
        next_offset: (end < all.len()).then_some(end),
        summaries: Some(page),
        ..base(catalog, "kind")
    }
}

fn names_view(catalog: &ArtCatalog, names: &[String], kind: Option<&str>) -> CatalogSlice {
    let mut entries: Vec<ArtCapability> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    let mut omitted: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut full = false;
    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        if seen.contains(&name.as_str()) {
            continue;
        }
        seen.push(name);
        let matches: Vec<&ArtCapability> = catalog
            .capabilities
            .iter()
            .filter(|c| c.name == *name && kind.is_none_or(|k| c.kind == k))
            .collect();
        if matches.is_empty() {
            not_found.push(name.clone());
            continue;
        }
        // Whole names only: a name is returned with all its kinds or not at all.
        let size: usize = matches
            .iter()
            .map(|c| serde_json::to_string(c).map_or(0, |s| s.len()))
            .sum();
        if full || (!entries.is_empty() && used + size > ENTRIES_BUDGET_BYTES) {
            full = true;
            omitted.push(name.clone());
            continue;
        }
        used += size;
        entries.extend(matches.into_iter().cloned());
    }
    CatalogSlice {
        kind: kind.map(str::to_string),
        entries: Some(entries),
        not_found: Some(not_found),
        omitted: Some(omitted),
        ..base(catalog, "names")
    }
}

impl CatalogSlice {
    /// A readable rendering of exactly what the structured form carries.
    pub fn text(&self) -> String {
        let mut out = format!("PSCat {} art catalog", self.pscat_version);
        if let Some(index) = &self.index {
            let total: usize = index.iter().map(|k| k.count).sum();
            out.push_str(&format!(": {total} capabilities by kind.\n"));
            if let Some(packs) = &self.font_packs {
                let installed: Vec<&str> =
                    packs.installed.iter().map(|p| p.name.as_str()).collect();
                out.push_str(&format!(
                    "Font packs installed: {}. Available: {}.\n",
                    if installed.is_empty() {
                        "none".to_string()
                    } else {
                        installed.join(", ")
                    },
                    packs.available.join(", ")
                ));
            }
            for k in index {
                out.push_str(&format!(
                    "\n{} ({}): {}",
                    k.kind,
                    k.count,
                    k.names.join(", ")
                ));
            }
            out.push_str(&format!(
                "\n\nDetails: call again with kind (one of {}) for descriptions, or with names \
                 (up to {MAX_NAMES}) for parameters, load lines and examples.",
                KINDS.join(", ")
            ));
        } else if let Some(summaries) = &self.summaries {
            let (kind, total, offset) = (
                self.kind.as_deref().unwrap_or(""),
                self.total.unwrap_or(0),
                self.offset.unwrap_or(0),
            );
            out.push_str(&format!(
                ": {kind}, {total} in all, listing {} from offset {offset}.\n",
                summaries.len()
            ));
            for s in summaries {
                out.push_str(&format!("\n{}: {}", s.name, s.description));
            }
            if let Some(next) = self.next_offset {
                out.push_str(&format!("\n\nMore: call again with offset {next}."));
            }
            out.push_str("\n\nDetails: call with names for parameters, load lines and examples.");
        } else {
            out.push_str(": full entries.\n");
            for c in self.entries.iter().flatten() {
                out.push_str(&format!(
                    "\n{} ({})\n  {}\n  source: {}\n  availability: {}\n",
                    c.name, c.kind, c.description, c.source, c.availability
                ));
                if !c.load.is_empty() {
                    out.push_str(&format!("  load: {}\n", c.load));
                }
                if !c.example.is_empty() {
                    out.push_str(&format!("  example: {}\n", c.example));
                }
                for p in &c.parameters {
                    let default = p
                        .default
                        .as_deref()
                        .map(|d| format!(" (default {d})"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "  parameter {}{default}: {}\n",
                        p.name, p.description
                    ));
                }
            }
            if let Some(nf) = self.not_found.as_ref().filter(|v| !v.is_empty()) {
                out.push_str(&format!("\nNot found: {}", nf.join(", ")));
            }
            if let Some(om) = self.omitted.as_ref().filter(|v| !v.is_empty()) {
                out.push_str(&format!(
                    "\nOmitted, did not fit in the reply (ask for them again): {}",
                    om.join(", ")
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::ArtParameter;

    fn cap(name: &str, kind: &str, description: &str) -> ArtCapability {
        ArtCapability {
            name: name.into(),
            kind: kind.into(),
            description: description.into(),
            parameters: vec![ArtParameter {
                name: "Seed".into(),
                description: "rand seed".into(),
                default: Some("1".into()),
            }],
            source: "lib/x.ps".into(),
            load: "(lib/x.ps) run".into(),
            example: "x".into(),
            availability: "library".into(),
        }
    }

    fn catalog(caps: Vec<ArtCapability>) -> ArtCatalog {
        serde_json::from_value(serde_json::json!({
            "pscat_version": "0.0.0",
            "catalog_signature": "0000000000000000",
            "font_packs": {"pinned_index": null, "installed": [], "overrides": [], "available": []},
            "capabilities": []
        }))
        .map(|mut c: ArtCatalog| {
            c.capabilities = caps;
            c
        })
        .unwrap()
    }

    #[test]
    fn rejects_bad_queries_before_any_work() {
        let q = |kind: Option<&str>, names: Option<Vec<&str>>, offset, limit| CatalogQuery {
            kind: kind.map(Into::into),
            names: names.map(|n| n.into_iter().map(Into::into).collect()),
            offset,
            limit,
        };
        assert!(q(None, None, None, None).check().is_ok());
        assert!(q(Some("font"), None, Some(5), Some(10)).check().is_ok());
        assert!(q(Some("fonts"), None, None, None).check().is_err());
        assert!(q(None, Some(vec![]), None, None).check().is_err());
        assert!(q(None, Some(vec!["a"; 21]), None, None).check().is_err());
        assert!(q(None, Some(vec!["a"; 20]), None, None).check().is_ok());
        assert!(q(None, None, Some(1), None).check().is_err());
        assert!(
            q(Some("font"), Some(vec!["a"]), None, Some(1))
                .check()
                .is_err()
        );
        assert!(q(Some("font"), None, None, Some(0)).check().is_err());
        assert!(q(Some("font"), None, None, Some(101)).check().is_err());
    }

    #[test]
    fn kind_pages_cut_descriptions_and_link_the_next_page() {
        let long = format!("{}\nsecond line", "é".repeat(300));
        let caps = (0..250)
            .map(|i| cap(&format!("p{i}"), "procedure", &long))
            .collect();
        let c = catalog(caps);
        let page = |offset, limit| {
            slice(
                &c,
                &CatalogQuery {
                    kind: Some("procedure".into()),
                    offset,
                    limit,
                    ..CatalogQuery::default()
                },
            )
            .unwrap()
        };
        let first = page(None, None);
        assert_eq!(first.total, Some(250));
        assert_eq!(first.summaries.as_ref().unwrap().len(), 100);
        assert_eq!(first.next_offset, Some(100));
        let d = &first.summaries.as_ref().unwrap()[0].description;
        assert_eq!(d.chars().count(), DESCRIPTION_CHARS + 1);
        assert!(d.ends_with('…') && !d.contains("second"));
        assert!(
            first.entries.is_none(),
            "kind alone returns no full entries"
        );
        let last = page(Some(200), Some(100));
        assert_eq!(last.summaries.as_ref().unwrap().len(), 50);
        assert_eq!(last.next_offset, None);
        assert!(page(Some(900), None).summaries.unwrap().is_empty());
        assert!(first.text().contains("offset 100"));
    }

    #[test]
    fn names_return_full_entries_and_report_what_is_missing() {
        let c = catalog(vec![
            cap("a", "procedure", "first"),
            cap("a", "font", "same name, another kind"),
            cap("b", "palette", "second"),
        ]);
        let ask = |names: &[&str], kind: Option<&str>| {
            slice(
                &c,
                &CatalogQuery {
                    names: Some(names.iter().map(|n| n.to_string()).collect()),
                    kind: kind.map(Into::into),
                    ..CatalogQuery::default()
                },
            )
            .unwrap()
        };
        let s = ask(&["b", "nope", "a", "b"], None);
        let got: Vec<(&str, &str)> = s
            .entries
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.kind.as_str()))
            .collect();
        assert_eq!(got, [("b", "palette"), ("a", "procedure"), ("a", "font")]);
        assert_eq!(s.not_found, Some(vec!["nope".to_string()]));
        assert_eq!(s.omitted, Some(vec![]));
        assert!(s.text().contains("Not found: nope"));
        assert!(s.text().contains("load: (lib/x.ps) run"));
        let only_fonts = ask(&["a", "b"], Some("font"));
        assert_eq!(only_fonts.entries.as_ref().unwrap().len(), 1);
        assert_eq!(only_fonts.not_found, Some(vec!["b".to_string()]));
    }

    #[test]
    fn names_stop_at_the_byte_budget_and_list_the_rest() {
        let big = "x".repeat(6000);
        let caps = (0..5)
            .map(|i| cap(&format!("n{i}"), "procedure", &big))
            .collect();
        let c = catalog(caps);
        let s = slice(
            &c,
            &CatalogQuery {
                names: Some((0..5).map(|i| format!("n{i}")).collect()),
                ..CatalogQuery::default()
            },
        )
        .unwrap();
        assert_eq!(s.entries.as_ref().unwrap().len(), 2);
        assert_eq!(s.omitted, Some(vec!["n2".into(), "n3".into(), "n4".into()]));
        assert!(s.text().contains("Omitted"));
        assert!(serde_json::to_string(&s).unwrap().len() < REPLY_BUDGET_BYTES);
        assert!(s.text().len() < REPLY_BUDGET_BYTES);
    }
}
