//! A wrapper for interacting with the `osc qam` command-line tool.
//!
//! Ported from upstream `mtui/data_sources/oscqam.py`. The [`Osc`] client drives
//! the SUSE `osc qam` plugin to **approve / assign / unassign / comment /
//! reject** a maintenance review, identified by its [`RequestReviewID`], against
//! the IBS API (`https://api.suse.de`).
//!
//! ## Faithful behaviour
//!
//! * **Injection-safe argv.** The command is built as a `Vec<String>`, never a
//!   shell string, so group names / messages / comments cannot inject shell
//!   syntax — exactly as upstream built a `list[str]` for `subprocess.run`.
//! * **stdin detached.** `osc` never inherits our stdin. Under `mtui-mcp` that
//!   stdin is the MCP stdio JSON-RPC pipe, so an interactive `osc` prompt (e.g.
//!   an approve confirmation) would block reading it forever and deadlock the
//!   server; feeding it null gives such a prompt EOF instead.
//! * **Time-capped.** A stalled `osc` can never wedge the session: the child is
//!   killed and [`OscError::Timeout`] returned after [`OSC_TIMEOUT_SECS`].
//! * **Output captured.** stdout/stderr are captured so a failure reports osc's
//!   actual reason (trimmed by [`tail`]) rather than a bare exit code.
//! * **`--skip-template`.** Added for `PI`/`SLFO` kinds on
//!   assign/approve/reject, whose RRID format the plugin does not expect by
//!   default.
//! * **`-G` headless hint.** A failed `-G/--group` call logs a hint to re-run
//!   without `-G`, whose confirmation prompt cannot be answered without a
//!   terminal (the interactive trap under `mtui-mcp`).
//!
//! ## Deviations from upstream (intentional)
//!
//! * **Async + injectable runner.** The blocking `subprocess.run` becomes an
//!   async [`CommandRunner`] trait; the production [`TokioRunner`] uses
//!   `tokio::process`, and tests inject a stub so they run fully offline without
//!   a real `osc` on `PATH`.
//! * **Typed `Result`.** Upstream folded every failure into a `bool`; this port
//!   returns `Result<(), `[`OscError`]`>` so the caller can inspect *why* an
//!   operation failed (the reason is still logged, preserving the behaviour).

use std::io;
use std::process::{Output, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use mtui_config::Config;
use mtui_types::{RequestKind, RequestReviewID};
use tokio::process::Command;

use crate::error::OscError;

/// The IBS API endpoint `osc qam` operates against (upstream `API`).
pub const API: &str = "https://api.suse.de";

/// Runtime cap for a single `osc` invocation, in seconds (upstream `timeout=180`).
pub const OSC_TIMEOUT_SECS: u64 = 180;

/// Maximum characters of captured osc output kept for logging / error detail.
const TAIL_LIMIT: usize = 2000;

/// Trim captured osc output to its last `limit` chars for logging.
///
/// Mirrors upstream `_tail`: strips surrounding whitespace and, when the text is
/// longer than `limit`, keeps only the tail (prefixed with an ellipsis). Returns
/// an empty string for empty input.
fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= limit {
        return trimmed.to_owned();
    }
    let tail: String = trimmed.chars().skip(count - limit).collect();
    format!("…{tail}")
}

/// The outcome of running an `osc` child process, abstracted so tests can inject
/// a canned result without spawning a real process.
///
/// The production implementation ([`TokioRunner`]) spawns `osc` with detached
/// stdin, captured output, and a hard timeout; a test stub records the argv it
/// was asked to run and returns a scripted [`RunOutcome`].
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args`, returning the captured process output.
    ///
    /// Implementations MUST detach the child's stdin, capture stdout/stderr, and
    /// enforce a timeout of `timeout` — the invocation-posture contract the
    /// [`Osc`] operations rely on.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<RunOutcome, RunError>;
}

/// A successful spawn-and-wait: the child ran to completion (cleanly or not).
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Whether the child exited with a success (zero) status.
    pub success: bool,
    /// The child's captured stdout, decoded lossily as UTF-8.
    pub stdout: String,
    /// The child's captured stderr, decoded lossily as UTF-8.
    pub stderr: String,
    /// The exit code, when one was reported (`None` if killed by a signal).
    pub code: Option<i32>,
}

/// A failure that prevented the child from running to completion.
#[derive(Debug)]
pub enum RunError {
    /// The child did not return within the timeout and was killed.
    Timeout,
    /// The program could not be found on `PATH`.
    NotFound,
    /// Any other I/O failure spawning or awaiting the child.
    Io(io::Error),
}

/// The production [`CommandRunner`]: spawns `osc` via `tokio::process`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioRunner;

#[async_trait]
impl CommandRunner for TokioRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<RunOutcome, RunError> {
        // Detach stdin (null), capture output; mirrors upstream stdin=DEVNULL,
        // capture_output=True. `kill_on_drop` is the backstop: if the timeout
        // future drops the child, the OS process is reaped rather than leaked.
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    RunError::NotFound
                } else {
                    RunError::Io(e)
                }
            })?;

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(outcome_from(&output)),
            Ok(Err(e)) => Err(RunError::Io(e)),
            Err(_elapsed) => {
                // The child outlived the runtime cap. `wait_with_output` owns the
                // child, so the elapsed `timeout` drops that future here;
                // `kill_on_drop(true)` ensures the still-running `osc` is killed
                // and reaped rather than leaked. Report the timeout.
                Err(RunError::Timeout)
            }
        }
    }
}

/// Build a [`RunOutcome`] from a finished process [`Output`].
fn outcome_from(output: &Output) -> RunOutcome {
    RunOutcome {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    }
}

/// A wrapper for interacting with the `osc qam` command-line tool.
///
/// Holds the target [`RequestReviewID`] and the [`CommandRunner`] used to
/// dispatch `osc`. The `config` is retained for parity with upstream (which
/// stored it) and future use.
pub struct Osc<R: CommandRunner = TokioRunner> {
    #[allow(dead_code)]
    config: Config,
    rrid: RequestReviewID,
    runner: R,
}

impl Osc<TokioRunner> {
    /// Build an [`Osc`] client that shells out to the real `osc` binary.
    ///
    /// Mirrors upstream `OSC.__init__(config, rrid)`.
    #[must_use]
    pub fn new(config: Config, rrid: RequestReviewID) -> Self {
        Self::with_runner(config, rrid, TokioRunner)
    }
}

impl<R: CommandRunner> Osc<R> {
    /// Build an [`Osc`] client with an explicit [`CommandRunner`].
    ///
    /// The test seam: inject a stub runner to exercise command-building and
    /// outcome handling without spawning a real process.
    pub fn with_runner(config: Config, rrid: RequestReviewID, runner: R) -> Self {
        Self {
            config,
            rrid,
            runner,
        }
    }

    /// Borrow the underlying [`CommandRunner`].
    ///
    /// Exposed so a caller (or test) can inspect the runner it injected — e.g. a
    /// stub that recorded the argv `osc` was asked to run.
    pub fn runner_ref(&self) -> &R {
        &self.runner
    }

    /// Construct and execute an `osc qam` command.
    ///
    /// Builds the argv in upstream's exact order:
    /// `osc -A <API> qam <operation> [-G <g>]... <review_id> [-R <reason>]
    /// [--skip-template] [-M <message>] [<comment>]`.
    async fn operation(
        &self,
        operation: &str,
        groups: &[String],
        reason: &str,
        message: &str,
        comment: &str,
    ) -> Result<(), OscError> {
        let mut command: Vec<String> = vec![
            "-A".to_owned(),
            API.to_owned(),
            "qam".to_owned(),
            operation.to_owned(),
        ];

        // -G <group> for each group.
        for g in groups {
            command.push("-G".to_owned());
            command.push(g.clone());
        }

        // review_id positional.
        command.push(self.rrid.review_id.to_string());

        // -R <reason>
        if !reason.is_empty() {
            command.push("-R".to_owned());
            command.push(reason.to_owned());
        }

        // --skip-template for PI/SLFO kinds on assign/approve/reject.
        if matches!(self.rrid.kind, RequestKind::Pi | RequestKind::Slfo)
            && matches!(operation, "assign" | "approve" | "reject")
        {
            command.push("--skip-template".to_owned());
        }

        // -M <message>
        if !message.is_empty() {
            command.push("-M".to_owned());
            command.push(message.to_owned());
        }

        // trailing comment positional.
        if !comment.is_empty() {
            command.push(comment.to_owned());
        }

        tracing::info!(operation, rrid = %self.rrid, "performing osc qam operation");
        tracing::debug!(command = ?command, "executing osc command");

        let timeout = Duration::from_secs(OSC_TIMEOUT_SECS);
        match self.runner.run("osc", &command, timeout).await {
            Ok(outcome) if outcome.success => {
                let out = tail(&outcome.stdout, TAIL_LIMIT);
                if !out.is_empty() {
                    tracing::info!(operation, output = %out, "osc operation succeeded");
                }
                Ok(())
            }
            Ok(outcome) => {
                let detail = {
                    let stderr = tail(&outcome.stderr, TAIL_LIMIT);
                    if !stderr.is_empty() {
                        stderr
                    } else {
                        let stdout = tail(&outcome.stdout, TAIL_LIMIT);
                        if !stdout.is_empty() {
                            stdout
                        } else {
                            match outcome.code {
                                Some(code) => format!("exit code {code}"),
                                None => "terminated by signal".to_owned(),
                            }
                        }
                    }
                };
                tracing::error!(operation, %detail, "osc operation failed");
                if !groups.is_empty() {
                    // `osc qam <op> -G` first asks an interactive confirmation;
                    // with stdin detached that prompt EOFs, so the -G path can
                    // only fail headless. The group review reassigned to you on
                    // pickup is a plain user review — act on it without -G.
                    tracing::error!(
                        operation,
                        "'{operation}' was called with -G/--group, whose osc confirmation \
                         prompt cannot be answered without a terminal (e.g. under mtui-mcp). \
                         Re-run '{operation}' without -G/--group to act on the review \
                         assigned to you.",
                    );
                }
                Err(OscError::NonZero {
                    operation: operation.to_owned(),
                    detail,
                })
            }
            Err(RunError::Timeout) => {
                tracing::error!(
                    operation,
                    "osc operation timed out after {OSC_TIMEOUT_SECS}s; osc did not return \
                     (likely an interactive prompt with no input)",
                );
                Err(OscError::Timeout {
                    operation: operation.to_owned(),
                    seconds: OSC_TIMEOUT_SECS,
                })
            }
            Err(RunError::NotFound) => {
                tracing::error!("'osc' command not found. Is it installed and in your PATH?");
                Err(OscError::NotFound)
            }
            Err(RunError::Io(source)) => {
                tracing::error!(operation, %source, "failed to run osc");
                Err(OscError::Runner {
                    operation: operation.to_owned(),
                    source,
                })
            }
        }
    }

    /// Approve a review request for one or more `groups`.
    ///
    /// # Errors
    ///
    /// Returns [`OscError`] if `osc` exits non-zero, times out, is missing, or
    /// otherwise fails to run.
    pub async fn approve(&self, groups: &[String]) -> Result<(), OscError> {
        self.operation("approve", groups, "", "", "").await
    }

    /// Assign a review request to one or more `groups`.
    ///
    /// # Errors
    ///
    /// Returns [`OscError`] if `osc` exits non-zero, times out, is missing, or
    /// otherwise fails to run.
    pub async fn assign(&self, groups: &[String]) -> Result<(), OscError> {
        self.operation("assign", groups, "", "", "").await
    }

    /// Unassign a review request from one or more `groups`.
    ///
    /// # Errors
    ///
    /// Returns [`OscError`] if `osc` exits non-zero, times out, is missing, or
    /// otherwise fails to run.
    pub async fn unassign(&self, groups: &[String]) -> Result<(), OscError> {
        self.operation("unassign", groups, "", "", "").await
    }

    /// Add a `comment` to a review request.
    ///
    /// # Errors
    ///
    /// Returns [`OscError`] if `osc` exits non-zero, times out, is missing, or
    /// otherwise fails to run.
    pub async fn comment(&self, comment: &str) -> Result<(), OscError> {
        self.operation("comment", &[], "", "", comment).await
    }

    /// Reject a review request for one or more `groups`, with a `reason` and
    /// `message`.
    ///
    /// # Errors
    ///
    /// Returns [`OscError`] if `osc` exits non-zero, times out, is missing, or
    /// otherwise fails to run.
    pub async fn reject(
        &self,
        groups: &[String],
        reason: &str,
        message: &str,
    ) -> Result<(), OscError> {
        self.operation("reject", groups, reason, message, "").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_empty_for_blank() {
        assert_eq!(tail("   \n  ", TAIL_LIMIT), "");
    }

    #[test]
    fn tail_strips_and_keeps_short_text() {
        assert_eq!(tail("  hello  ", TAIL_LIMIT), "hello");
    }

    #[test]
    fn tail_truncates_from_front_with_ellipsis() {
        let text: String = "x".repeat(10);
        let out = tail(&text, 4);
        assert_eq!(out, "…xxxx");
    }
}
