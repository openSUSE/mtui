//! A `cwd`-aware subprocess runner for `svn`.
//!
//! Test reports are stored in SVN (both OBS/IBS and SLFO incidents check the
//! template out from SVN — Gitea and `osc qam` are review-workflow backends
//! only, never a checkout mechanism). Every `svn` invocation is `cwd`-sensitive:
//! `svn co` runs in `template_dir` so the absolute repo URI lands in the right
//! place, and `svn ci`/`svn up` run inside the working copy.
//!
//! This is deliberately **not** the `mtui-datasources::oscqam::CommandRunner`:
//! that trait is an `osc`-review wrapper with an `osc`-tuned timeout posture and
//! **no `cwd` parameter**, and reusing it would couple testreport checkout to
//! the incident-review crate. A small, local trait keeps the crate boundary
//! honest and lets tests inject a stub that records the `cwd` (which
//! `test_svn_io.py` asserts).

use std::io;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

/// The captured result of a finished `svn` invocation.
#[derive(Debug, Clone)]
pub struct SvnOutcome {
    /// Whether the child exited with a success (zero) status.
    pub success: bool,
    /// The child's captured stderr, decoded lossily as UTF-8.
    ///
    /// Captured so `svn`'s cryptic `E170000: URL ... doesn't exist` line is
    /// surfaced at debug rather than reaching the user's terminal (upstream
    /// `svn_io.testreport_svn_checkout` passes `stderr=subprocess.PIPE`).
    pub stderr: String,
}

/// Runs `svn` subcommands in an explicit working directory.
///
/// The production implementation ([`TokioSvnRunner`]) spawns `svn` via
/// `tokio::process`; tests inject a stub that records the argv **and** `cwd` and
/// replays a scripted [`SvnOutcome`].
#[async_trait]
pub trait SvnRunner: Send + Sync {
    /// Run `svn` with `args` in working directory `cwd`, capturing stderr.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the child could not be spawned or
    /// awaited (e.g. `svn` is not installed).
    async fn run(&self, args: &[String], cwd: &Path) -> io::Result<SvnOutcome>;
}

/// The production [`SvnRunner`]: spawns `svn` via `tokio::process`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSvnRunner;

#[async_trait]
impl SvnRunner for TokioSvnRunner {
    async fn run(&self, args: &[String], cwd: &Path) -> io::Result<SvnOutcome> {
        // Detach stdin, capture stderr. `svn`'s absolute URI means we run with
        // an explicit `cwd` rather than mutating the process-global working
        // directory (mirrors upstream `cwd=config.template_dir`).
        let output = Command::new("svn")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;

        Ok(SvnOutcome {
            success: output.status.success(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
