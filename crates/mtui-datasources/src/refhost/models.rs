//! Refhost **search query** model, ported from
//! `mtui/hosts/refhost/models.py::Attributes` (+ its free parsing helpers).
//!
//! The `refhosts.yml` *row* schema ([`Host`], [`Product`], [`Addon`],
//! [`Version`]) lives in `mtui-types` (it is pure, I/O-free wire data). This
//! module adds the **query side**: [`Attributes`], the mutable search filter,
//! and [`Attributes::from_testplatform`], which parses a SMELT `testplatform`
//! string into one [`Attributes`] per architecture.
//!
//! # Divergence from upstream
//! Upstream keeps `Attributes` alongside `Host` in `models.py`. In mtui-rs the
//! `mtui-types` crate is deliberately the pure wire-schema and carries no
//! query/parsing concerns (and no `regex` dependency), so the query model and
//! its grammar parser live here in `mtui-datasources` next to the search engine
//! that consumes them (`store.rs`).

use std::sync::LazyLock;

use mtui_types::{Addon, Product, Version, version::VersionField};
use regex::Regex;

/// `arch=[a, b, c]` capture, anchored at start like upstream `re.match`.
static ARCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(.*)\]").expect("static arch regex is valid"));

/// `name(fields)` — greedy `.*` before `(` mirrors upstream `re.match`.
static NAMED_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*)\((.*)\)").expect("static named-version regex is valid"));

/// A search query against a [`Refhosts`](super::store::Refhosts) database.
///
/// Each field is optional; the matcher skips constraints on unset fields (an
/// empty `arch`, `product == None`, and empty `addons` are the "unset"
/// sentinels). Mirrors upstream's mutable `Attributes` dataclass — mutable
/// because [`from_testplatform`](Self::from_testplatform) builds it up segment
/// by segment before fanning out per arch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    /// The target architecture (empty string means "any").
    pub arch: String,
    /// The queried base product (`None` means "any").
    pub product: Option<Product>,
    /// The queried addons (all must be present on a candidate).
    pub addons: Vec<Addon>,
}

impl Attributes {
    /// Returns `true` when **no** constraint is set (upstream `__bool__`
    /// inverted). A search with an empty query matches nothing meaningful, so
    /// callers use this to short-circuit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.arch.is_empty() && self.product.is_none() && self.addons.is_empty()
    }

    /// Parse a SMELT `testplatform` string into one [`Attributes`] per arch.
    ///
    /// Grammar (segments separated by `;`):
    /// `base=<name>(major=X,minor=Y);arch=[a,b,c];addon=<name>(...)`.
    ///
    /// One [`Attributes`] is produced per architecture in the `arch=[…]`
    /// segment (the shared `base`/`addon` constraints are cloned into each).
    /// A segment with no `=` is logged at `error` and skipped; an unknown
    /// segment name (not `base`/`arch`/`addon`) is logged at `error` and
    /// skipped. With no `arch=[…]` segment, the result is empty (matching
    /// upstream, which fans out over an empty arch list).
    ///
    #[must_use]
    pub fn from_testplatform(testplatform: &str) -> Vec<Self> {
        let mut base = Self::default();
        let mut arch_list: Vec<String> = Vec::new();

        for segment in testplatform.split(';') {
            let Some((key, content)) = segment.split_once('=') else {
                tracing::error!(line = %testplatform, "refhost: error when parsing testplatform segment");
                continue;
            };

            match key {
                "arch" => {
                    if let Some(cap) = ARCH_RE.captures(content) {
                        arch_list = cap[1].split(',').map(|x| x.trim().to_owned()).collect();
                    }
                }
                "base" => {
                    if let Some((name, version)) = parse_named_version(content) {
                        base.product = Some(Product { name, version });
                    }
                }
                "addon" => {
                    if let Some((name, version)) = parse_named_version(content) {
                        base.addons.push(Addon { name, version });
                    }
                }
                other => {
                    tracing::error!(
                        segment = %other,
                        line = %testplatform,
                        "refhost: unknown testplatform segment",
                    );
                }
            }
        }

        arch_list
            .into_iter()
            .map(|arch| {
                let mut attr = base.clone();
                attr.arch = arch;
                attr
            })
            .collect()
    }
}

impl std::fmt::Display for Attributes {
    /// Human-readable query, e.g. `sles 15.5 x86_64 ha 15 sdk 15.5`.
    ///
    /// Mirrors upstream `__str__`: product first, then arch, then addons sorted
    /// alphabetically by name. Empty parts are omitted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();

        if let Some(product) = &self.product {
            parts.push(format_named_version(
                &product.name,
                product.version.as_ref(),
            ));
        }
        if !self.arch.is_empty() {
            parts.push(self.arch.clone());
        }

        let mut addons: Vec<&Addon> = self.addons.iter().collect();
        addons.sort_by(|a, b| a.name.cmp(&b.name));
        for addon in addons {
            parts.push(format_named_version(&addon.name, addon.version.as_ref()));
        }

        write!(
            f,
            "{}",
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

/// Render `name` + optional `version` like `"sles 15.5"` / `"sles 12sp4"`.
///
/// Ported from `_format_named_version`: a numeric minor uses a `.` separator, a
/// textual minor is concatenated, and the empty-string minor sentinel (or an
/// absent minor) renders as major only.
fn format_named_version(name: &str, version: Option<&Version>) -> String {
    let Some(version) = version else {
        return name.to_owned();
    };
    let mut out = format!("{name} {}", version.major);
    match &version.minor {
        Some(VersionField::Num(n)) => out.push_str(&format!(".{n}")),
        Some(VersionField::Text(s)) if !s.is_empty() => out.push_str(s),
        // `Text("")` (empty sentinel) and `None` render as major only.
        _ => {}
    }
    out
}

/// Parse `name(major=X,minor=Y)` → `(name, Some(Version))`.
///
/// Ported from `_parse_named_version`. Returns `None` when `content` does not
/// match the `name(...)` grammar at all. When it matches but has no `major`
/// field, returns `(name, None)`. Each field value is parsed as numeric when it
/// parses as an integer, else kept as text — so `minor=` yields the empty-string
/// [`VersionField::Text`] sentinel ("candidate must have no minor").
fn parse_named_version(content: &str) -> Option<(String, Option<Version>)> {
    let cap = NAMED_VERSION_RE.captures(content)?;
    let name = cap[1].to_owned();

    let mut major: Option<VersionField> = None;
    let mut minor: Option<VersionField> = None;
    for element in cap[2].split(',') {
        let Some((key, value)) = element.split_once('=') else {
            continue;
        };
        let field = parse_field(value);
        match key {
            "major" => major = Some(field),
            "minor" => minor = Some(field),
            _ => {}
        }
    }

    let Some(major) = major else {
        return Some((name, None));
    };
    Some((name, Some(Version { major, minor })))
}

/// Parse a single field value: numeric if it parses as `u64`, else textual.
///
/// `""` therefore becomes `VersionField::Text("")` — the "no minor" query
/// sentinel — matching upstream's `int(value)` / fallback-to-`str` behavior.
fn parse_field(value: &str) -> VersionField {
    match value.parse::<u64>() {
        Ok(n) => VersionField::Num(n),
        Err(_) => VersionField::Text(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: Option<VersionField>) -> Version {
        Version {
            major: VersionField::Num(major),
            minor,
        }
    }

    // --- Attributes Display / is_empty (test_refhost.py::TestAttributes) ---

    #[test]
    fn empty_attributes_is_empty_and_blank() {
        let attr = Attributes::default();
        assert!(attr.is_empty());
        assert_eq!(attr.to_string(), "");
    }

    #[test]
    fn str_with_product_major_only() {
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(v(12, None)),
            }),
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "sles 12");
    }

    #[test]
    fn str_with_product_int_minor_uses_dot() {
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(v(15, Some(VersionField::Num(5)))),
            }),
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "sles 15.5");
    }

    #[test]
    fn str_with_product_string_minor_concatenated() {
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(v(12, Some(VersionField::Text("sp4".to_owned())))),
            }),
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "sles 12sp4");
    }

    #[test]
    fn str_with_arch() {
        let attr = Attributes {
            arch: "x86_64".to_owned(),
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "x86_64");
    }

    #[test]
    fn str_with_addons_sorted() {
        let attr = Attributes {
            addons: vec![
                Addon {
                    name: "sdk".to_owned(),
                    version: Some(v(15, Some(VersionField::Num(5)))),
                },
                Addon {
                    name: "ha".to_owned(),
                    version: Some(v(15, None)),
                },
            ],
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "ha 15 sdk 15.5");
    }

    #[test]
    fn empty_string_minor_renders_as_major_only() {
        // The `minor=` sentinel must not leak into Display output.
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(v(11, Some(VersionField::Text(String::new())))),
            }),
            ..Default::default()
        };
        assert_eq!(attr.to_string(), "sles 11");
    }

    // --- from_testplatform (test_refhost.py::TestFromTestplatform) ---

    #[test]
    fn base_arch_addon_with_int_minor() {
        let tp = "base=sles(major=11,minor=4);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=4)";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(
            attrs.iter().map(|a| a.arch.as_str()).collect::<Vec<_>>(),
            ["i386", "s390x", "x86_64"]
        );
        for a in &attrs {
            assert_eq!(
                a.product,
                Some(Product {
                    name: "sles".to_owned(),
                    version: Some(v(11, Some(VersionField::Num(4)))),
                })
            );
            assert_eq!(
                a.addons,
                vec![Addon {
                    name: "sdk".to_owned(),
                    version: Some(v(11, Some(VersionField::Num(4)))),
                }]
            );
        }
    }

    #[test]
    fn addon_with_string_minor_kept_as_string() {
        let tp = "base=sles(major=12,minor=sp4);arch=[x86_64];addon=ha(major=12,minor=sp4)";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(attrs.len(), 1);
        let p = attrs[0].product.as_ref().unwrap();
        assert_eq!(
            p.version.as_ref().unwrap().minor,
            Some(VersionField::Text("sp4".to_owned()))
        );
        assert_eq!(
            attrs[0].addons[0].version.as_ref().unwrap().minor,
            Some(VersionField::Text("sp4".to_owned()))
        );
    }

    #[test]
    fn addon_with_empty_minor_is_sentinel() {
        let tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11,minor=)";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(
            attrs[0].addons[0].version,
            Some(v(11, Some(VersionField::Text(String::new()))))
        );
    }

    #[test]
    fn addon_major_only() {
        let tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11)";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(attrs[0].addons[0].version, Some(v(11, None)));
    }

    #[test]
    fn unknown_segment_skipped_rest_parses() {
        let tp = "base=sles(major=15,minor=5);arch=[x86_64];tags=(kernel)";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].product.as_ref().unwrap().name, "sles");
    }

    #[test]
    fn malformed_segment_skipped_rest_parses() {
        let tp = "garbage_no_equals;base=sles(major=15,minor=5);arch=[x86_64]";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].product.as_ref().unwrap().name, "sles");
    }

    #[test]
    fn no_arch_yields_empty_list() {
        let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5)");
        assert!(attrs.is_empty());
    }

    #[test]
    fn arch_list_with_spaces_is_trimmed() {
        let attrs = Attributes::from_testplatform("arch=[ x86_64 , aarch64 ]");
        assert_eq!(
            attrs.iter().map(|a| a.arch.as_str()).collect::<Vec<_>>(),
            ["x86_64", "aarch64"]
        );
    }

    #[test]
    fn arch_injection_is_literal() {
        // No eval — a shell-injection payload inside the brackets is parsed as a
        // plain literal arch string, never executed. (`;` is the segment
        // separator, so a payload with no embedded `;` stays in one segment.)
        let tp = "arch=[x' && rm -rf ~ && echo y]";
        let attrs = Attributes::from_testplatform(tp);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].arch, "x' && rm -rf ~ && echo y");
    }

    #[test]
    fn parse_named_version_no_parens_returns_none() {
        assert_eq!(parse_named_version("noparens"), None);
    }

    #[test]
    fn parse_named_version_no_major_returns_name_only() {
        assert_eq!(
            parse_named_version("sles()"),
            Some(("sles".to_owned(), None))
        );
    }
}
