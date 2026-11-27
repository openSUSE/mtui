//! The verdict gate `approve` and `reject` apply to a report document on
//! TeReGen v2, shared by the OBS and Gitea backends.

use std::fmt;

use mtui_types::RequestReviewID;
use mtui_types::report_document::Verdict;
use thiserror::Error;

use super::document::{DocumentFetch, TeregenV2};

/// The review decision a [`verdict_gate`] is asked to allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `approve`: needs verdict `PASSED`.
    Approve,
    /// `reject`: needs verdict `FAILED` and a non-blank top `comment`.
    Reject,
}

impl Decision {
    fn required(self) -> Verdict {
        match self {
            Self::Approve => Verdict::Passed,
            Self::Reject => Verdict::Failed,
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        })
    }
}

/// `null`, `PASSED` or `FAILED`, as the document spells it.
struct Label(Option<Verdict>);

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Some(Verdict::Passed) => "PASSED",
            Some(Verdict::Failed) => "FAILED",
            None => "null",
        })
    }
}

/// Why a [`verdict_gate`] refused a decision. Each `Display` is the refusal as
/// the user reads it, naming the next step.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VerdictRefusal {
    /// The server's copy could not be read: no document, a stale one, or a
    /// transport/status failure.
    #[error("cannot read the report document for {rrid} ({cause}); refusing to {decision}")]
    Unreadable {
        /// The report the document belongs to.
        rrid: String,
        /// The decision refused.
        decision: Decision,
        /// What went wrong.
        cause: String,
    },
    /// The verdict is not the one `decision` needs.
    #[error(
        "report document for {rrid} has verdict {found}, not {required}; set it and `commit` the \
         report first (refusing to {decision})",
        found = Label(*.found),
        required = Label(Some(.decision.required()))
    )]
    WrongVerdict {
        /// The report the document belongs to.
        rrid: String,
        /// The decision refused.
        decision: Decision,
        /// The verdict the document holds.
        found: Option<Verdict>,
    },
    /// A `FAILED` document without a reason to reject on.
    #[error(
        "report document for {rrid} has no comment; set one and `commit` the report \
         first (refusing to reject)"
    )]
    NoComment {
        /// The report the document belongs to.
        rrid: String,
    },
}

/// The report document's `verdict` and top-level `comment`, fetched fresh.
///
/// # Errors
///
/// The cause, ready to embed in a refusal: no document, a stale one, or the
/// transport/status failure.
async fn report_verdict(
    v2: &TeregenV2,
    rrid: &RequestReviewID,
) -> Result<(Option<Verdict>, Option<String>), String> {
    match v2.fetch_document(&rrid.to_string(), None).await {
        Ok(DocumentFetch::Fresh { document, .. }) => {
            Ok((document.verdict.0, document.comment.0.clone()))
        }
        Ok(DocumentFetch::NotModified) => {
            Err("the server answered 304 to an unconditional request".to_owned())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Whether TeReGen's copy of `rrid`'s report document, fetched fresh, allows
/// `decision`.
///
/// # Errors
///
/// The [`VerdictRefusal`] naming why not.
pub async fn verdict_gate(
    v2: &TeregenV2,
    rrid: &RequestReviewID,
    decision: Decision,
) -> Result<(), VerdictRefusal> {
    let (verdict, comment) =
        report_verdict(v2, rrid)
            .await
            .map_err(|cause| VerdictRefusal::Unreadable {
                rrid: rrid.to_string(),
                decision,
                cause,
            })?;
    if verdict != Some(decision.required()) {
        return Err(VerdictRefusal::WrongVerdict {
            rrid: rrid.to_string(),
            decision,
            found: verdict,
        });
    }
    if decision == Decision::Reject && comment.is_none_or(|c| c.trim().is_empty()) {
        return Err(VerdictRefusal::NoComment {
            rrid: rrid.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "SUSE:Maintenance:1:56789";

    /// The refusals are a user-facing contract shared by both backends: pin
    /// every one byte for byte.
    #[test]
    fn each_refusal_renders_its_exact_text() {
        let rrid = || ID.to_owned();
        for (refusal, text) in [
            (
                VerdictRefusal::Unreadable {
                    rrid: rrid(),
                    decision: Decision::Approve,
                    cause: "boom".to_owned(),
                },
                "cannot read the report document for SUSE:Maintenance:1:56789 (boom); \
                 refusing to approve",
            ),
            (
                VerdictRefusal::Unreadable {
                    rrid: rrid(),
                    decision: Decision::Reject,
                    cause: "boom".to_owned(),
                },
                "cannot read the report document for SUSE:Maintenance:1:56789 (boom); \
                 refusing to reject",
            ),
            (
                VerdictRefusal::WrongVerdict {
                    rrid: rrid(),
                    decision: Decision::Approve,
                    found: None,
                },
                "report document for SUSE:Maintenance:1:56789 has verdict null, not PASSED; \
                 set it and `commit` the report first (refusing to approve)",
            ),
            (
                VerdictRefusal::WrongVerdict {
                    rrid: rrid(),
                    decision: Decision::Reject,
                    found: Some(Verdict::Passed),
                },
                "report document for SUSE:Maintenance:1:56789 has verdict PASSED, not FAILED; \
                 set it and `commit` the report first (refusing to reject)",
            ),
            (
                VerdictRefusal::NoComment { rrid: rrid() },
                "report document for SUSE:Maintenance:1:56789 has no comment; set one and \
                 `commit` the report first (refusing to reject)",
            ),
        ] {
            assert_eq!(refusal.to_string(), text);
        }
    }
}
