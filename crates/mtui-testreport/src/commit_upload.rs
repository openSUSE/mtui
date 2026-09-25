//! Commit-time collection of a report's local artifacts and their upload
//! through teregen v2's document + artifact write clients — the `commit`
//! command's document path (SVN stays [`crate::checkout::svn_commit_testreport`]).
//!
//! [`collect_artifacts`] gathers the same sources `svn_commit_testreport`
//! adds (`install_logs/`, `results/`, `checkers.log`), entirely offline.
//! [`upload_current`] then sends the loaded document (conditional on its
//! stored `ETag`) followed by each artifact, sequentially — the server's
//! per-id `minion->guard($id, 60)` makes two concurrent writes to one id fail
//! the second with a `503`, so this is not an optimization opportunity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mtui_datasources::MAX_API_BODY;
use mtui_datasources::teregen::{
    ArtifactStored, ArtifactUploadError, Precondition, TeregenV2, TeregenV2WriteError,
    is_valid_artifact_name,
};
use thiserror::Error;

use crate::testreport::TestReportBase;

/// The artifacts [`collect_artifacts`] found: `files` in upload order (sorted
/// by name for determinism), `skipped` the non-regular-file entries seen in
/// an artifact source directory (subdirectories, symlinks — never followed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Collected {
    /// `(artifact name, source file path)`, sorted by name.
    pub files: Vec<(String, PathBuf)>,
    /// Paths skipped because they were not a regular file.
    pub skipped: Vec<PathBuf>,
}

/// Errors from [`collect_artifacts`] — every one is checked before any network
/// request is sent (A1).
#[derive(Debug, Error)]
pub enum CollectError {
    /// A file's basename is not a valid teregen artifact name (the server's
    /// `^[A-Za-z0-9_.-]+$`, not starting with `.`).
    #[error("{}: invalid artifact name {name:?}", path.display())]
    InvalidName {
        /// The offending file.
        path: PathBuf,
        /// Its basename.
        name: String,
    },
    /// Two files from different source directories share a basename.
    #[error("artifact name collision {name:?}: both {} and {}", a.display(), b.display())]
    Collision {
        /// The colliding name.
        name: String,
        /// The first file seen with this name.
        a: PathBuf,
        /// The second.
        b: PathBuf,
    },
    /// A file exceeds [`MAX_API_BODY`].
    #[error("{}: exceeds the {MAX_API_BODY}-byte limit", path.display())]
    TooLarge {
        /// The oversize file.
        path: PathBuf,
    },
    /// A filesystem operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path being read.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `true` when a file of `len` bytes would exceed [`MAX_API_BODY`] — also
/// `true` if `len` does not even fit a local `usize` (a 32-bit target), since
/// that is necessarily larger than the limit.
fn too_large(len: u64) -> bool {
    match usize::try_from(len) {
        Ok(len) => len > MAX_API_BODY,
        Err(_) => true,
    }
}

/// Adds every regular file directly under `dir` to `by_name` (keyed by
/// basename), pushing anything else (a subdirectory, a symlink) to `skipped`.
/// A missing `dir` contributes nothing. Uses `DirEntry`'s own (`lstat`-based)
/// file type, so a symlink is never followed to decide whether to include it.
fn collect_dir(
    dir: &Path,
    by_name: &mut BTreeMap<String, PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), CollectError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| CollectError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| CollectError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| CollectError::Io {
            path: path.clone(),
            source: e,
        })?;
        if !file_type.is_file() {
            skipped.push(path);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_valid_artifact_name(&name) {
            return Err(CollectError::InvalidName { path, name });
        }
        let len = entry
            .metadata()
            .map_err(|e| CollectError::Io {
                path: path.clone(),
                source: e,
            })?
            .len();
        if too_large(len) {
            return Err(CollectError::TooLarge { path });
        }
        if let Some(existing) = by_name.insert(name.clone(), path.clone()) {
            return Err(CollectError::Collision {
                name,
                a: existing,
                b: path,
            });
        }
    }
    Ok(())
}

/// Collects every regular file directly under `<report_wd>/<install_logs>/`,
/// under `<report_wd>/results/`, plus `<report_wd>/checkers.log` (6b-D1) — the
/// same sources [`crate::checkout::svn_commit_testreport`] adds. Absent
/// sources contribute nothing; a subdirectory or a symlink is skipped rather
/// than followed or erroring (A3).
///
/// # Errors
///
/// See [`CollectError`].
pub fn collect_artifacts(report_wd: &Path, install_logs: &Path) -> Result<Collected, CollectError> {
    let mut skipped = Vec::new();
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();

    collect_dir(&report_wd.join(install_logs), &mut by_name, &mut skipped)?;
    collect_dir(&report_wd.join("results"), &mut by_name, &mut skipped)?;

    let checkers_log = report_wd.join("checkers.log");
    match std::fs::symlink_metadata(&checkers_log) {
        Ok(meta) if meta.is_file() => {
            if too_large(meta.len()) {
                return Err(CollectError::TooLarge { path: checkers_log });
            }
            if let Some(existing) = by_name.insert("checkers.log".to_owned(), checkers_log.clone())
            {
                return Err(CollectError::Collision {
                    name: "checkers.log".to_owned(),
                    a: existing,
                    b: checkers_log,
                });
            }
        }
        Ok(_) => skipped.push(checkers_log),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CollectError::Io {
                path: checkers_log,
                source: e,
            });
        }
    }

    Ok(Collected {
        files: by_name.into_iter().collect(),
        skipped,
    })
}

/// The outcome of a successful [`upload_current`] call: the document's fresh
/// `ETag`, and each artifact's own upload result (a partial failure does not
/// fail the whole call — every artifact is attempted and reported).
#[derive(Debug)]
pub struct CommitReport {
    /// The document's `ETag` after the upload, as adopted onto
    /// [`TestReportBase::document_etag`].
    pub etag: Option<String>,
    /// `(artifact name, upload result)`, in the order artifacts were sent.
    pub artifacts: Vec<(String, Result<ArtifactStored, ArtifactUploadError>)>,
}

/// Errors from [`upload_current`] that abort before any artifact is sent — an
/// artifact-level failure is instead reported per-file in
/// [`CommitReport::artifacts`].
#[derive(Debug, Error)]
pub enum CommitUploadError {
    /// The report carries no loaded document, or no stored `ETag` (A2: mtui
    /// never falls back to an unconditional write).
    #[error("no document ETag on this report — reload before committing")]
    NoEtag,
    /// The document upload itself failed.
    #[error(transparent)]
    Document(#[from] TeregenV2WriteError),
}

/// Uploads `base`'s loaded document (conditional on its stored `ETag`), then
/// each of `artifacts` in order, sequentially — never concurrently (the
/// server's per-id guard would fail the second write with a `503`).
///
/// Only on a successful document upload are `base.document`/`document_etag`
/// adopted from the server's response (P5-D4); a `412`, or any other
/// document-upload failure, returns before touching `base` or sending a
/// single artifact.
///
/// # Errors
///
/// [`CommitUploadError::NoEtag`] when the report carries no document or no
/// stored `ETag`, with no request sent.
/// [`CommitUploadError::Document`] when the document upload itself fails.
pub async fn upload_current(
    base: &mut TestReportBase,
    client: &TeregenV2,
    artifacts: Vec<(String, PathBuf)>,
) -> Result<CommitReport, CommitUploadError> {
    let Some(document) = base.document.clone() else {
        return Err(CommitUploadError::NoEtag);
    };
    let Some(etag) = base.document_etag.clone() else {
        return Err(CommitUploadError::NoEtag);
    };
    let id = document.id.clone();

    let outcome = client
        .upload_document(&id, &document, &Precondition::Match(etag))
        .await?;
    let new_etag = outcome.etag;
    base.document = Some(*outcome.document);
    base.document_etag = new_etag.clone();

    let mut results = Vec::with_capacity(artifacts.len());
    for (name, path) in artifacts {
        let outcome = match tokio::fs::read(&path).await {
            Ok(bytes) => client.upload_artifact(&id, &name, bytes).await,
            // A read failure here is a rare TOCTOU (the file vanished or lost
            // permissions between `collect_artifacts` and this upload, since
            // `collect_artifacts` already proved it stat'd as a regular
            // file): no distinct variant exists for a local read failure, so
            // it is reported the same way any other failure to *send* this
            // artifact is.
            Err(e) => Err(ArtifactUploadError::Transport(format!(
                "reading {}: {e}",
                path.display()
            ))),
        };
        results.push((name, outcome));
    }

    Ok(CommitReport {
        etag: new_etag,
        artifacts: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn collects_install_logs_results_and_checkers_log() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "install_logs/h1.log", b"a");
        write(wd, "results/summary.txt", b"b");
        write(wd, "checkers.log", b"c");

        let collected = collect_artifacts(wd, Path::new("install_logs")).unwrap();
        let names: Vec<&str> = collected.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["checkers.log", "h1.log", "summary.txt"]);
        assert!(collected.skipped.is_empty());
    }

    #[test]
    fn missing_sources_yield_an_empty_result_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let collected = collect_artifacts(tmp.path(), Path::new("install_logs")).unwrap();
        assert!(collected.files.is_empty());
        assert!(collected.skipped.is_empty());
    }

    #[test]
    fn subdir_and_symlink_are_skipped_not_uploaded() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        std::fs::create_dir_all(wd.join("install_logs/nested")).unwrap();
        let real = write(wd, "outside.log", b"x");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, wd.join("install_logs/link.log")).unwrap();

        let collected = collect_artifacts(wd, Path::new("install_logs")).unwrap();
        assert!(collected.files.is_empty());
        #[cfg(unix)]
        assert_eq!(collected.skipped.len(), 2, "{:?}", collected.skipped);
        #[cfg(not(unix))]
        assert_eq!(collected.skipped.len(), 1, "{:?}", collected.skipped);
    }

    #[test]
    fn collision_between_install_logs_and_results_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "install_logs/dup.log", b"a");
        write(wd, "results/dup.log", b"b");

        let err = collect_artifacts(wd, Path::new("install_logs")).unwrap_err();
        assert!(matches!(err, CollectError::Collision { name, .. } if name == "dup.log"));
    }

    #[test]
    fn invalid_name_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "install_logs/bad name!.log", b"a");

        let err = collect_artifacts(wd, Path::new("install_logs")).unwrap_err();
        assert!(matches!(err, CollectError::InvalidName { name, .. } if name == "bad name!.log"));
    }

    #[test]
    fn oversize_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "install_logs/huge.log", &vec![0u8; MAX_API_BODY + 1]);

        let err = collect_artifacts(wd, Path::new("install_logs")).unwrap_err();
        assert!(matches!(err, CollectError::TooLarge { .. }));
    }

    #[test]
    fn oversize_checkers_log_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "checkers.log", &vec![0u8; MAX_API_BODY + 1]);

        let err = collect_artifacts(wd, Path::new("install_logs")).unwrap_err();
        assert!(matches!(err, CollectError::TooLarge { .. }));
    }

    #[test]
    fn checkers_log_colliding_with_install_logs_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        write(wd, "install_logs/checkers.log", b"a");
        write(wd, "checkers.log", b"b");

        let err = collect_artifacts(wd, Path::new("install_logs")).unwrap_err();
        assert!(matches!(err, CollectError::Collision { name, .. } if name == "checkers.log"));
    }

    /// The rare TOCTOU: `collect_artifacts` has already proved a path is a
    /// regular file, but `upload_current` reads it just in time and the file
    /// has since vanished. That single artifact fails with the local read
    /// error, reported the same way any other failed artifact send is —
    /// the document upload itself still succeeds.
    #[tokio::test]
    async fn upload_current_reports_a_vanished_file_as_a_failed_artifact() {
        use mtui_config::options::Config;
        use mtui_datasources::teregen::{ArtifactUploadError, TeregenAuth, TeregenV2, TokenStore};
        use mtui_datasources::{HttpClient, VerifyPolicy};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let key = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obs/id_ed25519");
        Mock::given(method("POST"))
            .and(path("/auth/ssh/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nonce": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "a".repeat(64),
            })))
            .mount(&server)
            .await;

        let raw = r#"{
            "schema_version": "1.0", "id": "x", "kind": "pi",
            "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {"packager": "p", "source_packages": ["a"], "origin": {},
                       "products": [{"name": "n", "version": "v", "archs": ["x86_64"]}],
                       "patches": [{"id": "1", "title": "t"}]},
            "install": {"repository": "http://x/", "targets": [{
                "product": "n", "version": "v", "arch": "x86_64",
                "repository": "http://x/r", "binaries": {"a": "1-1.x86_64"}
            }], "test_platforms": []},
            "issues": {}, "testing": {}
        }"#;
        let doc: mtui_types::report_document::ReportDocument = raw.parse().unwrap();
        Mock::given(method("PUT"))
            .and(path("/reports/x"))
            .respond_with(
                ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
            )
            .mount(&server)
            .await;

        let mut base = TestReportBase::new(Config::default());
        base.document = Some(doc);
        base.document_etag = Some("\"x\"".to_owned());

        let tmp = tempfile::tempdir().unwrap();
        let store_file = tmp.path().join("teregen-token.json");
        let auth = TeregenAuth::new(
            server.uri(),
            "alice".to_owned(),
            Some(key),
            None,
            HttpClient::new(VerifyPolicy::Default(true)).unwrap(),
        )
        .with_store(Some(TokenStore::at(store_file)));
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let client = TeregenV2::with_client(http, &server.uri()).with_auth(auth);

        let missing = tmp.path().join("does-not-exist.log");
        let report = upload_current(&mut base, &client, vec![("gone.log".to_owned(), missing)])
            .await
            .expect("the document upload still succeeds");

        assert_eq!(report.artifacts.len(), 1);
        assert!(
            matches!(&report.artifacts[0].1, Err(ArtifactUploadError::Transport(msg)) if msg.contains("reading")),
            "{:?}",
            report.artifacts[0]
        );
    }
}
