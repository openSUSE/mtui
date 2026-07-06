//! Ports `tests/test_updateid_checkout.py`: `UpdateID._checkout` error mapping.
//!
//! The seam reads a template; a missing template (ENOENT) triggers an `svn`
//! checkout, and any checkout failure is mapped to the same clean
//! `TestReportNotLoaded` error instead of escaping as a raw error.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use mtui_testreport::{
    CheckoutError, CheckoutRunError, ReadOutcome, TemplateIoError, checkout_and_read,
};

/// An unusable `template_dir` surfaces as `TestReportNotLoaded`.
///
/// The read raises ENOENT (no template on disk); the checkout then fails with
/// `TemplateDirNotUsable` (e.g. a plain file in the way). `_checkout` must map
/// it to `TestReportNotLoaded` and must have attempted the checkout exactly once.
#[tokio::test]
async fn checkout_unusable_template_dir_maps_to_not_loaded() {
    let trpath = PathBuf::from("/nonexistent/SUSE:Maintenance:1:1/log");
    let checkout_calls = AtomicUsize::new(0);

    // The report read always reports "no template on disk" (ENOENT).
    let read = |_p: &std::path::Path| {
        ReadOutcome::Io(TemplateIoError::from_io(&io::Error::from(
            io::ErrorKind::NotFound,
        )))
    };

    let checkout = async || {
        checkout_calls.fetch_add(1, Ordering::SeqCst);
        Err(CheckoutError::TemplateDirNotUsable {
            path: "/nonexistent/templates".to_owned(),
            reason: "in the way".to_owned(),
        })
    };

    let err = checkout_and_read(&trpath, read, checkout)
        .await
        .unwrap_err();

    assert!(
        matches!(err, CheckoutRunError::NotLoaded(_)),
        "expected TestReportNotLoaded mapping, got: {err:?}"
    );
    assert_eq!(checkout_calls.load(Ordering::SeqCst), 1);
}

/// A template already on disk is read without any checkout attempt.
#[tokio::test]
async fn checkout_reads_existing_template_without_checkout() {
    let trpath = PathBuf::from("/some/report/log");
    let checkout_calls = AtomicUsize::new(0);

    let read = |_p: &std::path::Path| ReadOutcome::Ok;
    let checkout = async || {
        checkout_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };

    checkout_and_read(&trpath, read, checkout)
        .await
        .expect("existing template should load");
    assert_eq!(checkout_calls.load(Ordering::SeqCst), 0);
}

/// If the retry read still fails after a successful checkout, the seam surfaces
/// `TestReportNotLoaded` rather than the raw read error.
#[tokio::test]
async fn checkout_succeeds_but_retry_read_fails_maps_to_not_loaded() {
    let trpath = PathBuf::from("/some/report/log");
    let read_calls = AtomicUsize::new(0);

    // Both reads report ENOENT; the checkout "succeeded" but produced nothing
    // readable, so the second read still misses.
    let read = |_p: &std::path::Path| {
        read_calls.fetch_add(1, Ordering::SeqCst);
        ReadOutcome::Io(TemplateIoError::from_io(&io::Error::from(
            io::ErrorKind::NotFound,
        )))
    };
    let checkout = async || Ok(());

    let err = checkout_and_read(&trpath, read, checkout)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CheckoutRunError::NotLoaded(_)),
        "retry read failure should map to TestReportNotLoaded, got: {err:?}"
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), 2);
}

/// A non-ENOENT read error propagates unchanged (never triggers a checkout).
#[tokio::test]
async fn checkout_non_enoent_read_error_propagates() {
    let trpath = PathBuf::from("/some/report/log");
    let checkout_calls = AtomicUsize::new(0);

    let read = |_p: &std::path::Path| {
        ReadOutcome::Io(TemplateIoError::from_io(&io::Error::from(
            io::ErrorKind::PermissionDenied,
        )))
    };
    let checkout = async || {
        checkout_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };

    let err = checkout_and_read(&trpath, read, checkout)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CheckoutRunError::Read(_)),
        "non-ENOENT read error should propagate, got: {err:?}"
    );
    assert_eq!(checkout_calls.load(Ordering::SeqCst), 0);
}

/// A successful checkout followed by a good read loads the report.
#[tokio::test]
async fn checkout_then_successful_read_loads() {
    let trpath = PathBuf::from("/some/report/log");
    let read_calls = AtomicUsize::new(0);

    // First read: ENOENT (missing). Second read (after checkout): Ok.
    let read = |_p: &std::path::Path| {
        if read_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ReadOutcome::Io(TemplateIoError::from_io(&io::Error::from(
                io::ErrorKind::NotFound,
            )))
        } else {
            ReadOutcome::Ok
        }
    };
    let checkout = async || Ok(());

    checkout_and_read(&trpath, read, checkout)
        .await
        .expect("checkout + read should load");
    assert_eq!(read_calls.load(Ordering::SeqCst), 2);
}
