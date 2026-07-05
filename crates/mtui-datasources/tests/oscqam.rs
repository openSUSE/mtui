//! Integration tests for the `osc qam` subprocess wrapper.
//!
//! Ports the behavioral core of upstream `tests/test_oscqam.py`. Upstream
//! patched `subprocess.run`; here we inject a [`StubRunner`] implementing
//! [`CommandRunner`], which records the argv it was asked to run and returns a
//! scripted outcome — so the tests run fully offline without a real `osc` on
//! `PATH`.
//!
//! Mapping from the Python test classes:
//!
//! * `TestOSCCommandBuilding` → the `builds_*` / `*_command` tests below assert
//!   the recorded argv (operation, `-G` per group, `--skip-template` gating,
//!   `-R`/`-M`, `-A <API>`).
//! * `TestOSCInvocation` → the runner is always called with `osc`, and the
//!   production runner's stdin/capture/timeout posture is documented in the
//!   crate module (the trait contract); the timeout value is asserted here.
//! * `TestOSCOutcome` → `Ok(())` on success; `Err(OscError::…)` on non-zero /
//!   timeout / missing binary (never a panic).
//! * `TestOSCErrorReporting` → the non-zero detail carries osc's stderr; the
//!   `-G` failure path is distinguished from the no-group path via the error
//!   variant + the group presence.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use mtui_config::Config;
use mtui_datasources::OscError;
use mtui_datasources::oscqam::{CommandRunner, OSC_TIMEOUT_SECS, Osc, RunError, RunOutcome};
use mtui_types::{RequestKind, RequestReviewID};

/// A recording of one `run` invocation.
#[derive(Debug, Clone)]
struct Recorded {
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

/// A [`CommandRunner`] stub: records each invocation and replays a scripted
/// result. The default result is a clean success with empty output.
struct StubRunner {
    calls: Mutex<Vec<Recorded>>,
    result: Mutex<Result<RunOutcome, StubErr>>,
}

/// A cloneable stand-in for [`RunError`] (which is not `Clone`).
#[derive(Debug, Clone)]
enum StubErr {
    Timeout,
    NotFound,
    Io,
}

impl StubRunner {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(RunOutcome {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
                code: Some(0),
            })),
        }
    }

    fn with_result(result: Result<RunOutcome, StubErr>) -> Self {
        let stub = Self::new();
        *stub.result.lock().unwrap() = result;
        stub
    }

    fn last_args(&self) -> Vec<String> {
        self.calls.lock().unwrap().last().unwrap().args.clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn last(&self) -> Recorded {
        self.calls.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl CommandRunner for StubRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<RunOutcome, RunError> {
        self.calls.lock().unwrap().push(Recorded {
            program: program.to_owned(),
            args: args.to_vec(),
            timeout,
        });
        match self.result.lock().unwrap().clone() {
            Ok(outcome) => Ok(outcome),
            Err(StubErr::Timeout) => Err(RunError::Timeout),
            Err(StubErr::NotFound) => Err(RunError::NotFound),
            Err(StubErr::Io) => Err(RunError::Io(std::io::Error::other("boom"))),
        }
    }
}

fn rrid(kind: RequestKind) -> RequestReviewID {
    RequestReviewID {
        project: "SUSE".to_owned(),
        kind,
        maintenance_id: "1".to_owned(),
        review_id: 2,
    }
}

fn osc_with(kind: RequestKind, runner: StubRunner) -> Osc<StubRunner> {
    Osc::with_runner(Config::default(), rrid(kind), runner)
}

fn group(s: &str) -> Vec<String> {
    vec![s.to_owned()]
}

// ---- TestOSCCommandBuilding -------------------------------------------------

#[tokio::test]
async fn approve_builds_correct_command() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();

    let args = osc.runner_ref().last_args();
    assert!(args.contains(&"qam".to_owned()));
    assert!(args.contains(&"approve".to_owned()));
    assert!(args.contains(&"-G".to_owned()));
    assert!(args.contains(&"qam-sle".to_owned()));
    assert_eq!(osc.runner_ref().last().program, "osc");
}

#[tokio::test]
async fn assign_builds_correct_command() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.assign(&group("qam-sle")).await.unwrap();
    assert!(osc.runner_ref().last_args().contains(&"assign".to_owned()));
}

#[tokio::test]
async fn unassign_builds_correct_command() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.unassign(&group("qam-sle")).await.unwrap();
    assert!(
        osc.runner_ref()
            .last_args()
            .contains(&"unassign".to_owned())
    );
}

#[tokio::test]
async fn reject_includes_reason_and_message() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.reject(&group("qam-sle"), "bug found", "details here")
        .await
        .unwrap();

    let args = osc.runner_ref().last_args();
    assert!(args.contains(&"reject".to_owned()));
    assert!(args.contains(&"-R".to_owned()));
    assert!(args.contains(&"bug found".to_owned()));
    assert!(args.contains(&"-M".to_owned()));
    assert!(args.contains(&"details here".to_owned()));
}

#[tokio::test]
async fn comment_builds_correct_command() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.comment("test comment").await.unwrap();
    let args = osc.runner_ref().last_args();
    assert!(args.contains(&"comment".to_owned()));
    assert!(args.contains(&"test comment".to_owned()));
}

#[tokio::test]
async fn multiple_groups_add_g_for_each() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&["qam-sle".to_owned(), "qam-kernel".to_owned()])
        .await
        .unwrap();

    let args = osc.runner_ref().last_args();
    let g_count = args.iter().filter(|x| *x == "-G").count();
    assert_eq!(g_count, 2);
}

#[tokio::test]
async fn empty_groups_omit_g() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.comment("hello").await.unwrap();
    assert!(!osc.runner_ref().last_args().contains(&"-G".to_owned()));
}

#[tokio::test]
async fn skip_template_added_for_pi() {
    let osc = osc_with(RequestKind::Pi, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();
    assert!(
        osc.runner_ref()
            .last_args()
            .contains(&"--skip-template".to_owned())
    );
}

#[tokio::test]
async fn skip_template_added_for_slfo() {
    let osc = osc_with(RequestKind::Slfo, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();
    assert!(
        osc.runner_ref()
            .last_args()
            .contains(&"--skip-template".to_owned())
    );
}

#[tokio::test]
async fn no_skip_template_for_maintenance() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();
    assert!(
        !osc.runner_ref()
            .last_args()
            .contains(&"--skip-template".to_owned())
    );
}

#[tokio::test]
async fn no_skip_template_for_pi_comment() {
    // --skip-template only gates on assign/approve/reject, not comment.
    let osc = osc_with(RequestKind::Pi, StubRunner::new());
    osc.comment("hi").await.unwrap();
    assert!(
        !osc.runner_ref()
            .last_args()
            .contains(&"--skip-template".to_owned())
    );
}

#[tokio::test]
async fn api_url_is_passed() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();

    let args = osc.runner_ref().last_args();
    let api_index = args.iter().position(|x| x == "-A").expect("-A present");
    assert_eq!(args[api_index + 1], "https://api.suse.de");
}

#[tokio::test]
async fn review_id_positional_present() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();
    // review_id is 2 for the test rrid.
    assert!(osc.runner_ref().last_args().contains(&"2".to_owned()));
}

// ---- TestOSCInvocation ------------------------------------------------------

#[tokio::test]
async fn runner_invoked_with_osc_and_timeout() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    osc.approve(&group("qam-sle")).await.unwrap();

    let rec = osc.runner_ref().last();
    assert_eq!(rec.program, "osc");
    assert_eq!(rec.timeout, Duration::from_secs(OSC_TIMEOUT_SECS));
    assert_eq!(osc.runner_ref().call_count(), 1);
}

// ---- TestOSCOutcome ---------------------------------------------------------

#[tokio::test]
async fn returns_ok_on_success() {
    let osc = osc_with(RequestKind::Maintenance, StubRunner::new());
    assert!(osc.approve(&group("qam-sle")).await.is_ok());
}

#[tokio::test]
async fn success_with_stdout_still_ok() {
    // A clean exit that also printed a confirmation line: still Ok, and the
    // success-output logging path runs.
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: true,
        stdout: "Approving 414975 for tester (qam-sle).".to_owned(),
        stderr: String::new(),
        code: Some(0),
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    assert!(osc.approve(&group("qam-sle")).await.is_ok());
}

#[tokio::test]
async fn failure_detail_reports_signal_termination() {
    // Non-zero with no code (killed by signal) and no captured output ->
    // the detail is the signal-termination fallback.
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: String::new(),
        stderr: String::new(),
        code: None,
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    match osc.comment("hi").await.unwrap_err() {
        OscError::NonZero { detail, .. } => assert_eq!(detail, "terminated by signal"),
        other => panic!("expected NonZero, got {other:?}"),
    }
}

#[tokio::test]
async fn returns_nonzero_error_on_failure() {
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: String::new(),
        stderr: "boom".to_owned(),
        code: Some(1),
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.approve(&group("qam-sle")).await.unwrap_err();
    assert!(matches!(err, OscError::NonZero { .. }));
}

#[tokio::test]
async fn returns_timeout_error() {
    let runner = StubRunner::with_result(Err(StubErr::Timeout));
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.approve(&group("qam-sle")).await.unwrap_err();
    assert!(matches!(err, OscError::Timeout { seconds, .. } if seconds == OSC_TIMEOUT_SECS));
}

#[tokio::test]
async fn returns_not_found_error() {
    let runner = StubRunner::with_result(Err(StubErr::NotFound));
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.approve(&group("qam-sle")).await.unwrap_err();
    assert!(matches!(err, OscError::NotFound));
}

#[tokio::test]
async fn returns_runner_error_on_io() {
    let runner = StubRunner::with_result(Err(StubErr::Io));
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.approve(&group("qam-sle")).await.unwrap_err();
    assert!(matches!(err, OscError::Runner { .. }));
}

// ---- TestOSCErrorReporting --------------------------------------------------

#[tokio::test]
async fn failure_surfaces_osc_stderr() {
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: String::new(),
        stderr: "Error: request 414975 already accepted".to_owned(),
        code: Some(1),
    }));
    // no group -> isolates the stderr-surfacing path.
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.comment("hi").await.unwrap_err();
    match err {
        OscError::NonZero { detail, .. } => assert!(detail.contains("already accepted")),
        other => panic!("expected NonZero, got {other:?}"),
    }
}

#[tokio::test]
async fn failure_detail_falls_back_to_stdout_then_code() {
    // Empty stderr + non-empty stdout -> detail is stdout.
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: "stdout reason".to_owned(),
        stderr: String::new(),
        code: Some(3),
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    match osc.comment("hi").await.unwrap_err() {
        OscError::NonZero { detail, .. } => assert_eq!(detail, "stdout reason"),
        other => panic!("expected NonZero, got {other:?}"),
    }

    // Empty stderr + empty stdout -> detail is the exit code.
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: String::new(),
        stderr: String::new(),
        code: Some(7),
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    match osc.comment("hi").await.unwrap_err() {
        OscError::NonZero { detail, .. } => assert_eq!(detail, "exit code 7"),
        other => panic!("expected NonZero, got {other:?}"),
    }
}

#[tokio::test]
async fn error_display_mentions_operation() {
    let runner = StubRunner::with_result(Ok(RunOutcome {
        success: false,
        stdout: String::new(),
        stderr: "nope".to_owned(),
        code: Some(1),
    }));
    let osc = osc_with(RequestKind::Maintenance, runner);
    let err = osc.approve(&group("qam-sle")).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("approve"));
    assert!(msg.contains("nope"));
}
