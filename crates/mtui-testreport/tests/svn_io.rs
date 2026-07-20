//! Ports `tests/test_svn_io.py`: the `svn` checkout/commit subprocess helpers.
//!
//! The production runner spawns `svn`; here a [`StubSvnRunner`] records the argv
//! **and** `cwd` it was asked to run and replays a scripted outcome, so the
//! tests run fully offline and can assert the exact invocation posture.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use mtui_config::options::Config;
use mtui_testreport::{
    CheckoutError, SvnOutcome, SvnRunner, svn_commit_testreport, testreport_svn_checkout,
};
use mtui_types::RequestReviewID;

/// A single recorded `svn` invocation.
#[derive(Debug, Clone)]
struct Recorded {
    args: Vec<String>,
    cwd: PathBuf,
}

/// A [`SvnRunner`] stub: records each invocation and replays a scripted result.
/// The default result is a clean success with empty stderr.
struct StubSvnRunner {
    calls: Mutex<Vec<Recorded>>,
    /// Scripted outcomes, consumed front-to-back; a `None` element means "return
    /// a spawn error". When exhausted, falls back to a clean success.
    script: Mutex<Vec<Option<SvnOutcome>>>,
}

impl StubSvnRunner {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(Vec::new()),
        }
    }

    fn with_outcome(outcome: SvnOutcome) -> Self {
        let stub = Self::new();
        stub.script.lock().unwrap().push(Some(outcome));
        stub
    }

    /// A stub whose first invocation returns a spawn error (e.g. `svn` missing).
    fn spawn_error() -> Self {
        let stub = Self::new();
        stub.script.lock().unwrap().push(None);
        stub
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn last(&self) -> Recorded {
        self.calls.lock().unwrap().last().unwrap().clone()
    }

    fn all(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SvnRunner for StubSvnRunner {
    async fn run(&self, args: &[String], cwd: &Path) -> io::Result<SvnOutcome> {
        self.calls.lock().unwrap().push(Recorded {
            args: args.to_vec(),
            cwd: cwd.to_path_buf(),
        });
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Ok(SvnOutcome {
                success: true,
                stderr: String::new(),
            });
        }
        match script.remove(0) {
            Some(outcome) => Ok(outcome),
            None => Err(io::Error::other("svn not found")),
        }
    }
}

fn cfg(template_dir: PathBuf) -> Config {
    let mut c = Config::default();
    c.template_dir = template_dir;
    c.fancy_reports_url = "https://qam.suse.de/reports".to_owned();
    c
}

/// A non-existent report yields a clear message pointing to the log URL, with
/// `svn`'s own cryptic error code suppressed.
#[tokio::test]
async fn svn_checkout_missing_raises_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let config = cfg(tmp.path().to_path_buf());
    let rrid = RequestReviewID::parse("SUSE:SLFO:1.2:9999").unwrap();
    let runner = StubSvnRunner::with_outcome(SvnOutcome {
        success: false,
        stderr: "svn: E170000: URL '...' doesn't exist\n".to_owned(),
    });

    let err = testreport_svn_checkout(
        &runner,
        &config,
        "svn+ssh://svn@qam.suse.de/testreports",
        &rrid,
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("Test report for SUSE:SLFO:1.2:9999 does not exist"),
        "message was: {msg}"
    );
    assert!(
        msg.contains("https://qam.suse.de/reports/SUSE:SLFO:1.2:9999/log"),
        "message was: {msg}"
    );
    // The cryptic svn error code is not part of the user-facing message.
    assert!(!msg.contains("E170000"), "message leaked svn code: {msg}");
    assert!(matches!(err, CheckoutError::SvnCheckoutFailed { .. }));

    // The checkout ran `svn co <uri>` in the template_dir.
    let call = runner.last();
    assert_eq!(
        call.args,
        vec![
            "co".to_owned(),
            "svn+ssh://svn@qam.suse.de/testreports/SUSE:SLFO:1.2:9999".to_owned(),
        ]
    );
    assert_eq!(call.cwd, tmp.path());
}

/// A successful checkout returns without raising.
#[tokio::test]
async fn svn_checkout_success_does_not_raise() {
    let tmp = tempfile::tempdir().unwrap();
    let config = cfg(tmp.path().to_path_buf());
    let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    let runner = StubSvnRunner::with_outcome(SvnOutcome {
        success: true,
        stderr: String::new(),
    });

    testreport_svn_checkout(&runner, &config, "svn+ssh://svn@example/testreports", &rrid)
        .await
        .expect("successful checkout should not error");
}

/// A `template_dir` that cannot be created is one actionable error, and `svn co`
/// is never invoked.
#[tokio::test]
async fn svn_checkout_unusable_template_dir_raises_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    // A plain file sitting where `template_dir` points cannot be a directory.
    let blocked = tmp.path().join("templates");
    std::fs::write(&blocked, "a file, not a directory").unwrap();
    let config = cfg(blocked.clone());
    let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    let runner = StubSvnRunner::new();

    let err = testreport_svn_checkout(&runner, &config, "svn+ssh://svn@example/testreports", &rrid)
        .await
        .unwrap_err();

    assert!(matches!(err, CheckoutError::TemplateDirNotUsable { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains(&blocked.display().to_string()),
        "message should name the offending dir: {msg}"
    );
    assert!(
        msg.contains("[mtui] template_dir option"),
        "message should point at the config option: {msg}"
    );
    // svn co is never run when the template_dir is unusable.
    assert_eq!(runner.call_count(), 0);
}

/// A spawn failure (e.g. `svn` not installed) is surfaced as a failed checkout.
#[tokio::test]
async fn svn_checkout_spawn_error_is_failed_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let config = cfg(tmp.path().to_path_buf());
    let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    let runner = StubSvnRunner::spawn_error();

    let err = testreport_svn_checkout(&runner, &config, "svn+ssh://svn@example/testreports", &rrid)
        .await
        .unwrap_err();
    assert!(matches!(err, CheckoutError::SvnCheckoutFailed { .. }));
}

/// A non-zero `svn` call inside the commit sequence aborts the commit.
#[tokio::test]
async fn svn_commit_aborts_on_failed_call() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path();
    let install_logs = checkout.join("install_logs");
    // First call (`svn add --force <install_logs>`) fails.
    let runner = StubSvnRunner::with_outcome(SvnOutcome {
        success: false,
        stderr: "svn: E200009: add failed\n".to_owned(),
    });

    let err = svn_commit_testreport(&runner, checkout, &install_logs, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, CheckoutError::SvnCheckoutFailed { .. }));
    // The sequence stopped after the failing add.
    assert_eq!(runner.call_count(), 1);
}

/// A spawn failure inside the commit sequence also aborts the commit.
#[tokio::test]
async fn svn_commit_aborts_on_spawn_error() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path();
    let install_logs = checkout.join("install_logs");
    let runner = StubSvnRunner::spawn_error();

    let err = svn_commit_testreport(&runner, checkout, &install_logs, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, CheckoutError::SvnCheckoutFailed { .. }));
}

/// `svn_commit_testreport` issues the exact upstream argv sequence, skipping the
/// optional `results` / `checkers.log` adds when those paths are absent.
#[tokio::test]
async fn svn_commit_minimal_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path();
    let install_logs = checkout.join("install_logs");
    let runner = StubSvnRunner::new();

    svn_commit_testreport(&runner, checkout, &install_logs, &[])
        .await
        .expect("commit should succeed");

    let calls: Vec<Vec<String>> = runner.all().into_iter().map(|c| c.args).collect();
    assert_eq!(
        calls,
        vec![
            vec![
                "add".to_owned(),
                "--force".to_owned(),
                install_logs.display().to_string(),
            ],
            vec!["up".to_owned()],
            vec!["ci".to_owned()],
        ]
    );
    // Every call ran in the checkout working copy.
    for c in runner.all() {
        assert_eq!(c.cwd, checkout);
    }
}

/// When `results/` and `checkers.log` exist, both optional adds are issued, and
/// commit-message args are appended to `svn ci`.
#[tokio::test]
async fn svn_commit_full_sequence_with_msg() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path();
    let install_logs = checkout.join("install_logs");
    std::fs::create_dir(checkout.join("results")).unwrap();
    std::fs::write(checkout.join("checkers.log"), "log").unwrap();
    let runner = StubSvnRunner::new();

    let msg = vec!["-m".to_owned(), "done".to_owned()];
    svn_commit_testreport(&runner, checkout, &install_logs, &msg)
        .await
        .expect("commit should succeed");

    let calls: Vec<Vec<String>> = runner.all().into_iter().map(|c| c.args).collect();
    assert_eq!(
        calls,
        vec![
            vec![
                "add".to_owned(),
                "--force".to_owned(),
                install_logs.display().to_string(),
            ],
            vec!["add".to_owned(), "--force".to_owned(), "results".to_owned()],
            vec![
                "add".to_owned(),
                "--force".to_owned(),
                "checkers.log".to_owned(),
            ],
            vec!["up".to_owned()],
            vec!["ci".to_owned(), "-m".to_owned(), "done".to_owned()],
        ]
    );
}
