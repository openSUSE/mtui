//! The error family for the native OBS/IBS backend.
//!
//! Ported from upstream `mtui/data_sources/obs/errors.py` (the internal
//! transport/timeout subtypes) plus the caller-facing `ObsError` /
//! `ObsConfigError` base from `mtui/support/exceptions.py`. Upstream splits
//! these across a class hierarchy so the `OSC` facade can catch the *whole*
//! family with one `except ObsError` and fold it into a logged `False`. This
//! port collapses that hierarchy into a single [`ObsError`] enum: one type the
//! facade matches exhaustively, mirroring the crate's other typed error
//! families ([`crate::error::GiteaError`], [`crate::error::OscError`]).
//!
//! The transport foundation (G1a) landed [`Api`](ObsError::Api),
//! [`Timeout`](ObsError::Timeout) and the [`Http`](ObsError::Http) transport
//! passthrough; the oscrc reader (G1b) added [`Config`](ObsError::Config)
//! (upstream `ObsConfigError`); the XML models (G1d) added
//! [`Parse`](ObsError::Parse) (malformed OBS XML and the DTD/XXE refusal).
//! Later subtasks widen the enum with `Inference` (assignment state machine,
//! G1e) variants; `#[non_exhaustive]` keeps that additive.

use thiserror::Error;

use crate::error::HttpError;

/// The OBS backend error family.
///
/// The `OSC(config, rrid)` facade (landing in G1g) converts *every* member of
/// this family into a logged `false`, so its bare callers never see a panic.
/// Keeping it one enum means that facade catch is a single `match`/`Err(_)`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObsError {
    /// An OBS API call returned a non-2xx HTTP response.
    ///
    /// Reproduces upstream `ObsApiError`: the message is
    /// `OBS API returned {status} for {url}` with a `": {summary}"` suffix only
    /// when the `<status><summary>` error envelope carried a non-empty summary.
    /// The `status`/`url`/`summary` are kept as inspectable fields so callers
    /// (and later ops) can branch on them.
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
    /// Reproduces upstream `ObsTimeoutError`. A whole operation makes a few
    /// calls; the deadline is checked *before* each one (there is no safe
    /// in-process mid-call hard kill), so the payload names the URL the budget
    /// was exhausted before.
    #[error("{0}")]
    Timeout(String),

    /// A configuration/credential fault while reading `~/.oscrc` (or, later, the
    /// SSH-signature signer, G1c).
    ///
    /// Reproduces upstream `ObsConfigError`: a fail-closed, secret-safe message
    /// that names the real failing oscrc file/section. The native oscrc reader
    /// ([`crate::obs::oscrc`]) never interpolates a parser error's own text into
    /// this message, so a malformed oscrc's offending source line (possibly a
    /// password) is never leaked.
    #[error("{0}")]
    Config(String),

    /// A malformed OBS XML payload, or a payload refused by the DTD/XXE guard.
    ///
    /// Reproduces upstream `models.py`'s bare `ObsError(msg)`: both a reader
    /// failure and the pre-parse `<!DOCTYPE`/`<!ENTITY` refusal raise the same
    /// base exception, so the `OSC` facade folds either into a logged `false`
    /// with one `Err(_)` arm. The DTD-refusal message contains `"DTD"`,
    /// matching upstream's `pytest.raises(ObsError, match="DTD")`.
    #[error("{0}")]
    Parse(String),

    /// A transport failure, a non-2xx surfaced by the shared HTTP layer, or a
    /// client-build failure (e.g. an unreadable CA bundle).
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Render the `": {summary}"` suffix for [`ObsError::Api`], empty when the
/// summary is empty — matching upstream `ObsApiError.__init__`'s
/// `detail = f": {summary}" if summary else ""`.
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
        // Mirrors upstream ObsApiError message with a non-empty summary.
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
        // No trailing ": " when the error envelope had no summary.
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
        // Mirrors upstream ObsConfigError: a plain, fail-closed message.
        let e = ObsError::Config("oscrc [https://api.suse.de] has no 'user'".to_owned());
        assert_eq!(e.to_string(), "oscrc [https://api.suse.de] has no 'user'");
    }

    #[test]
    fn parse_error_display_is_verbatim_message() {
        // Mirrors upstream models.py's bare ObsError(msg): plain, verbatim.
        let e = ObsError::Parse("refusing to parse an OBS document that carries a DTD".to_owned());
        assert_eq!(
            e.to_string(),
            "refusing to parse an OBS document that carries a DTD"
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
