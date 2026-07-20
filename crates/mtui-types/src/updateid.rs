//! Update identifier, ported from `mtui/types/updateid.py`.
//!
//! Upstream `UpdateID` is an abstract base that bundles a [`RequestReviewID`]
//! with a `TestReport` factory and a VCS-checkout callable; its concrete
//! `OBSUpdateID(rrid)` constructor simply parses the RRID string into a
//! [`RequestReviewID`] and stores it (`id_ = RequestReviewID(rrid)`), then wires
//! up the I/O collaborators.
//!
//! This is the **value-type slice only**: an [`UpdateID`] wrapping the parsed
//! [`RequestReviewID`], constructed by parsing an RRID string. The TestReport
//! factory, the SVN/Gitea checkout, and the interactive prompter are deferred to
//! a later phase (the update workflow lives in `mtui-testreport` / `mtui-core`),
//! keeping this crate I/O-free.
//!
//! Because the RRID grammar is an interop **Contract** (see `AGENTS.md`),
//! [`UpdateID::parse`] delegates verbatim to [`RequestReviewID::parse`], and
//! [`Display`](fmt::Display) forwards to the inner RRID so that path
//! construction downstream (upstream builds `template_dir/<str(uid.id)>`) stays
//! byte-for-byte consistent between the Rust and Python implementations.

use std::fmt;
use std::str::FromStr;

use crate::error::RridParseError;
use crate::rrid::RequestReviewID;

/// A parsed update identifier: a thin wrapper over the update's
/// [`RequestReviewID`].
///
/// Construct one with [`UpdateID::parse`] or via [`FromStr`]. The inner RRID is
/// normalised on parse (see [`RequestReviewID`]), so structural equality and
/// hashing match upstream's string-identity semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpdateID {
    /// The Request Review ID naming this update.
    pub id: RequestReviewID,
}

impl UpdateID {
    /// Parses an update identifier from an RRID string.
    ///
    /// Mirrors upstream `OBSUpdateID(rrid)`, whose `__init__` does
    /// `id_ = RequestReviewID(rrid)`. The I/O collaborators upstream attaches
    /// at the same point (TestReport factory, VCS checkout) are deferred to a
    /// later phase.
    ///
    /// # Errors
    ///
    /// Returns the [`RridParseError`] produced by [`RequestReviewID::parse`]
    /// for a malformed RRID string.
    pub fn parse(rrid: &str) -> Result<Self, RridParseError> {
        Ok(Self {
            id: RequestReviewID::parse(rrid)?,
        })
    }
}

impl FromStr for UpdateID {
    type Err = RridParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for UpdateID {
    /// Renders the inner RRID's canonical string, matching upstream's
    /// `str(uid.id)` used for the per-update template directory name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.id, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::RequestKind;

    #[test]
    fn parse_stores_inner_rrid_and_display_round_trips() {
        let uid = UpdateID::parse("SUSE:Maintenance:1:2").unwrap();
        assert_eq!(uid.id.project, "SUSE");
        assert_eq!(uid.id.kind, RequestKind::Maintenance);
        assert_eq!(uid.id.maintenance_id, "1");
        assert_eq!(uid.id.review_id, 2);
        // Display forwards to the inner RRID's canonical string.
        assert_eq!(uid.to_string(), "SUSE:Maintenance:1:2");
    }

    #[test]
    fn parse_delegates_error_to_rrid() {
        let err = UpdateID::parse("SUSE:Maintenance:1:2:3")
            .expect_err("expected too-many-components failure");
        assert!(
            matches!(err, RridParseError::TooManyComponents { limit: 4 }),
            "got {err:?}"
        );
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let uid: UpdateID = "S:M:1:2".parse().unwrap();
        // Short project form normalises via the inner RRID parser.
        assert_eq!(uid.id.project, "SUSE");
        assert_eq!(uid.to_string(), "SUSE:Maintenance:1:2");
    }
}
