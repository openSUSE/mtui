//! Re-fetches a loaded report's v2 document and re-applies it onto the report
//! — the document-path half of `checkout`.
//!
//! A refresh that does not end in [`Refreshed::Updated`] leaves the report
//! untouched, so a failed or unchanged fetch can never half-apply.

use mtui_datasources::teregen::{DocumentFetch, TeregenV2, TeregenV2Error};
use thiserror::Error;

use crate::ingest::apply_document;
use crate::testreport::TestReport;

/// What [`refresh_document`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refreshed {
    /// The server answered `304`: the loaded document is still current.
    Unchanged,
    /// A newer document was adopted; `etag` is its `ETag`, when sent.
    Updated {
        /// The adopted document's `ETag`.
        etag: Option<String>,
    },
}

/// Errors from [`refresh_document`].
#[derive(Debug, Error)]
pub enum RefreshError {
    /// The report was not loaded from a document, so there is nothing to
    /// refresh.
    #[error("this report was not loaded from a document")]
    NoDocument,
    /// The fetch was refused or failed.
    #[error(transparent)]
    Fetch(#[from] TeregenV2Error),
}

/// Fetches `report`'s document conditional on its stored `ETag`, or
/// unconditionally when `force` is set — the caller is discarding local edits,
/// so a `304` ("the server's copy is unchanged") must not keep them. On a newer
/// document, adopts it and its `ETag`, re-applies it onto the report,
/// re-derives `update_repos`, and clears `document_dirty`.
///
/// # Errors
///
/// [`RefreshError::NoDocument`] when the report carries no document, with no
/// request sent. [`RefreshError::Fetch`] when the server refuses or the
/// transport fails; the report is left unchanged.
pub async fn refresh_document(
    report: &mut (dyn TestReport + Send + Sync),
    client: &TeregenV2,
    force: bool,
) -> Result<Refreshed, RefreshError> {
    let Some(id) = report.base().document.as_ref().map(|d| d.id.clone()) else {
        return Err(RefreshError::NoDocument);
    };
    let etag = report.base().document_etag.clone().filter(|_| !force);

    match client.fetch_document(&id, etag.as_deref()).await? {
        DocumentFetch::NotModified => Ok(Refreshed::Unchanged),
        DocumentFetch::Fresh { document, etag, .. } => {
            apply_document(report.base_mut(), &document);
            let base = report.base_mut();
            base.document = Some(*document);
            base.document_etag.clone_from(&etag);
            base.document_dirty = false;
            let repos = report.update_repos_parser();
            report.base_mut().update_repos = repos;
            Ok(Refreshed::Updated { etag })
        }
    }
}
