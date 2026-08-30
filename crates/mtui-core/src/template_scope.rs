//! The single "which one template does this invocation address" rule.
//!
//! Three surfaces need the same answer to "one template, or refuse": the core
//! fan-out driver's `Scope::Explicit`/`Scope::Active` headless path, and the two
//! hand-written MCP tool families (`transfer_tools::resolve_rrid`,
//! `testreport_tools::resolve_path`) that bypass the fan-out driver entirely.
//! Before this module the rule was hand-copied twice and had already diverged
//! in wording; [`Session::resolve_single_template`] and
//! [`ambiguous_template_message`] are the one implementation all three consume.

use crate::session::Session;

/// How many loaded RRIDs [`ambiguous_template_message`] names before
/// collapsing the rest into a count.
const AMBIGUOUS_TEMPLATE_DISPLAY_CAP: usize = 8;

/// Which single template an invocation addresses, or why it cannot be decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleTemplate {
    /// The template to act on: named, the sole one loaded, or the addressable
    /// active one.
    One(String),
    /// Nothing is loaded. Not itself an error: the core dispatcher answers off
    /// the null report, while the MCP tools refuse — the caller's own verdict.
    NothingLoaded,
    /// Several templates are loaded, none was named, and no active pointer is
    /// addressable (headless).
    Ambiguous(Vec<String>),
    /// The named rrid is not in the registry.
    NotLoaded(String),
}

impl Session {
    /// Resolves `named` (or the fallback) to a single template.
    ///
    /// `use_active`: may an unaddressed call fall back to the active pointer?
    /// The REPL passes `true` (`switch` makes the active pointer addressable
    /// and the prompt displays it); every headless surface passes `false`.
    #[must_use]
    pub fn resolve_single_template(&self, named: Option<&str>, use_active: bool) -> SingleTemplate {
        if let Some(rrid) = named {
            return if self.templates.contains(rrid) {
                SingleTemplate::One(rrid.to_owned())
            } else {
                SingleTemplate::NotLoaded(rrid.to_owned())
            };
        }

        let rrids = self.templates.rrids();
        match rrids.len() {
            0 => SingleTemplate::NothingLoaded,
            1 => SingleTemplate::One(rrids.into_iter().next().expect("len checked above")),
            _ if use_active => {
                // A registry with more than one entry always has an active
                // one (`TemplateRegistry::add` sets it on the first insert).
                SingleTemplate::One(self.templates.active_rrid().unwrap_or_default().to_owned())
            }
            _ => SingleTemplate::Ambiguous(rrids),
        }
    }
}

/// The single wording for "several loaded, none named".
///
/// `remedy` is the surface's own escape hatch appended after the loaded list —
/// the escapes genuinely differ (`-T`/`--all-templates` for the core dispatcher,
/// `template=<rrid>` for the MCP tools) so it is not folded in here. `loaded` is
/// capped at a fixed number of entries plus a total count, so a session with
/// dozens of templates loaded does not spell every rrid out.
#[must_use]
pub fn ambiguous_template_message(loaded: &[String], remedy: &str) -> String {
    let list = if loaded.len() > AMBIGUOUS_TEMPLATE_DISPLAY_CAP {
        format!(
            "{}, … ({} total)",
            loaded[..AMBIGUOUS_TEMPLATE_DISPLAY_CAP].join(", "),
            loaded.len()
        )
    } else {
        loaded.join(", ")
    };
    format!("more than one template is loaded ({list}); {remedy}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report};

    fn rrid(n: u32) -> String {
        format!("SUSE:Maintenance:{n}:{n}")
    }

    #[test]
    fn named_and_loaded_resolves_to_that_one() {
        let (mut session, _buf) = empty_session();
        session.templates.add(fake_report(&rrid(1), &["h1"], "ok"));
        session.templates.add(fake_report(&rrid(2), &["h2"], "ok"));

        assert_eq!(
            session.resolve_single_template(Some(&rrid(2)), false),
            SingleTemplate::One(rrid(2))
        );
    }

    #[test]
    fn named_but_not_loaded_is_not_loaded() {
        let (session, _buf) = empty_session();
        assert_eq!(
            session.resolve_single_template(Some("bogus"), false),
            SingleTemplate::NotLoaded("bogus".to_owned())
        );
    }

    #[test]
    fn nothing_loaded_is_its_own_variant() {
        let (session, _buf) = empty_session();
        assert_eq!(
            session.resolve_single_template(None, true),
            SingleTemplate::NothingLoaded
        );
        assert_eq!(
            session.resolve_single_template(None, false),
            SingleTemplate::NothingLoaded
        );
    }

    #[test]
    fn sole_loaded_template_resolves_without_naming_it() {
        let (mut session, _buf) = empty_session();
        session.templates.add(fake_report(&rrid(1), &["h1"], "ok"));

        assert_eq!(
            session.resolve_single_template(None, false),
            SingleTemplate::One(rrid(1))
        );
    }

    #[test]
    fn several_loaded_with_use_active_picks_the_active_one_silently() {
        let (mut session, _buf) = empty_session();
        session.templates.add(fake_report(&rrid(1), &["h1"], "ok"));
        session.templates.add(fake_report(&rrid(2), &["h2"], "ok"));
        // The first added is active; anti-vacuity that this test observes the
        // *active* pointer, not just "the first" by coincidence.
        assert!(session.templates.set_active(&rrid(2)));

        assert_eq!(
            session.resolve_single_template(None, true),
            SingleTemplate::One(rrid(2))
        );
    }

    #[test]
    fn several_loaded_without_use_active_is_ambiguous() {
        let (mut session, _buf) = empty_session();
        session.templates.add(fake_report(&rrid(1), &["h1"], "ok"));
        session.templates.add(fake_report(&rrid(2), &["h2"], "ok"));

        assert_eq!(
            session.resolve_single_template(None, false),
            SingleTemplate::Ambiguous(vec![rrid(1), rrid(2)])
        );
    }

    #[test]
    fn ambiguous_message_lists_every_rrid_under_the_cap() {
        let loaded = vec![rrid(1), rrid(2), rrid(3)];
        assert_eq!(
            ambiguous_template_message(&loaded, "pass `template=<rrid>`"),
            "more than one template is loaded (SUSE:Maintenance:1:1, SUSE:Maintenance:2:2, \
             SUSE:Maintenance:3:3); pass `template=<rrid>`"
        );
    }

    #[test]
    fn ambiguous_message_caps_a_long_list() {
        let loaded: Vec<String> = (1..=18).map(rrid).collect();
        let msg = ambiguous_template_message(&loaded, "pass `template=<rrid>`");
        assert!(
            msg.contains("… (18 total)"),
            "expected a capped count; got: {msg}"
        );
        assert!(
            !msg.contains(&rrid(9)),
            "the 9th rrid is past the 8-entry cap and must not appear; got: {msg}"
        );
        assert!(
            msg.contains(&rrid(8)),
            "the 8th rrid is at the cap boundary and must appear"
        );
    }
}
