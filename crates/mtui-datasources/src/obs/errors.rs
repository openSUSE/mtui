//! The error family for the native OBS/IBS backend.
//!
//! One [`ObsError`] enum covers every failure in the backend — transport,
//! timeout, config/credential, XML parse and workflow-precondition — so the
//! `OSC` facade can match it exhaustively with a single `Err(_)` arm and fold
//! it into a logged `false`, as it does for [`crate::error::GiteaError`].
//! `#[non_exhaustive]` keeps further additions additive.

use thiserror::Error;

use crate::error::HttpError;

/// The OBS backend error family.
///
/// The `OSC(config, rrid)` facade converts *every* member of this family into a
/// logged `false`, so its bare callers never see a panic — one enum keeps that
/// catch a single `Err(_)` arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObsError {
    /// An OBS API call returned a non-2xx HTTP response.
    ///
    /// `OBS API returned {status} for {url}`, with a `": {summary}"` suffix only
    /// when the `<status><summary>` error envelope carried one. The fields stay
    /// inspectable so callers can branch on them.
    #[error("OBS API returned {status} for {url}{}", summary_suffix(summary))]
    Api {
        /// The HTTP status code of the failing response.
        status: u16,
        /// The request URL that produced the error.
        url: String,
        /// The parsed `<status><summary>` text, or empty when absent/unparseable.
        summary: String,
    },

    /// A native OBS operation exceeded its coarse between-calls time budget.
    ///
    /// An operation makes a few calls and the deadline is checked *before* each
    /// (there is no safe in-process mid-call hard kill), so the payload names
    /// the URL the budget was exhausted before.
    #[error("{0}")]
    Timeout(String),

    /// A configuration/credential fault from the oscrc or the SSH-signature
    /// signer.
    ///
    /// A fail-closed, secret-safe message naming the failing oscrc
    /// file/section. [`crate::obs::oscrc`] never interpolates a parser error's
    /// own text here, so a malformed oscrc's offending source line — possibly a
    /// password — is never leaked.
    #[error("{0}")]
    Config(String),

    /// A malformed OBS XML payload, or a payload refused by the DTD/XXE guard.
    ///
    /// A reader failure and the pre-parse `<!DOCTYPE`/`<!ENTITY` refusal share
    /// this variant, so one `Err(_)` arm folds either into a logged `false`. The
    /// DTD-refusal message contains `"DTD"`.
    #[error("{0}")]
    Parse(String),

    /// A QAM operation refused a workflow precondition: an empty comment, an
    /// ambiguous auto-inferred group, a request not open for review, a
    /// missing/wrong-`SUMMARY` testreport, the previous-decline guard, or the
    /// group-approve refusal.
    ///
    /// Distinct from [`Parse`](ObsError::Parse) (malformed XML) so the `OSC`
    /// facade can tell a workflow refusal apart from a transport/parse fault
    /// while still folding both into a logged `false`.
    #[error("{0}")]
    Op(String),

    /// A transport failure, a non-2xx surfaced by the shared HTTP layer, or a
    /// client-build failure (e.g. an unreadable CA bundle).
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Render the `": {summary}"` suffix for [`ObsError::Api`], empty when the
/// summary is empty.
fn summary_suffix(summary: &str) -> String {
    if summary.is_empty() {
        String::new()
    } else {
        format!(": {summary}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_display_includes_summary_suffix() {
        let e = ObsError::Api {
            status: 404,
            url: "https://api.suse.de/request/9".to_owned(),
            summary: "Request 9 not found".to_owned(),
        };
        assert_eq!(
            e.to_string(),
            "OBS API returned 404 for https://api.suse.de/request/9: Request 9 not found"
        );
    }

    #[test]
    fn api_error_display_omits_suffix_when_summary_empty() {
        let e = ObsError::Api {
            status: 500,
            url: "https://api.suse.de/x".to_owned(),
            summary: String::new(),
        };
        assert_eq!(
            e.to_string(),
            "OBS API returned 500 for https://api.suse.de/x"
        );
    }

    #[test]
    fn timeout_display_is_verbatim_message() {
        let e = ObsError::Timeout(
            "OBS operation exceeded its between-calls time budget before \
             https://api.suse.de/request/1"
                .to_owned(),
        );
        assert_eq!(
            e.to_string(),
            "OBS operation exceeded its between-calls time budget before \
             https://api.suse.de/request/1"
        );
    }

    #[test]
    fn config_error_display_is_verbatim_message() {
        let e = ObsError::Config("oscrc [https://api.suse.de] has no 'user'".to_owned());
        assert_eq!(e.to_string(), "oscrc [https://api.suse.de] has no 'user'");
    }

    #[test]
    fn parse_error_display_is_verbatim_message() {
        let e = ObsError::Parse("refusing to parse an OBS document that carries a DTD".to_owned());
        assert_eq!(
            e.to_string(),
            "refusing to parse an OBS document that carries a DTD"
        );
    }

    #[test]
    fn op_error_display_is_verbatim_message() {
        let e =
            ObsError::Op("group approval is not supported by the native OBS backend".to_owned());
        assert_eq!(
            e.to_string(),
            "group approval is not supported by the native OBS backend"
        );
    }

    #[test]
    fn http_error_is_transparent() {
        // The transport passthrough keeps the underlying message verbatim.
        let inner = HttpError::CaBundle {
            path: "/x/ca.pem".to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        };
        let inner_msg = inner.to_string();
        let e = ObsError::from(inner);
        assert_eq!(e.to_string(), inner_msg);
    }
}
