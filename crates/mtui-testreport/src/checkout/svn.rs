//! SVN subprocess helpers, ported from `mtui/test_reports/svn_io.py`.

use std::path::Path;

use mtui_config::options::Config;
use mtui_types::RequestReviewID;
use tracing::debug;

use super::CheckoutError;
use super::runner::SvnRunner;

/// Checks out a test report template from SVN.
///
/// Ports upstream `testreport_svn_checkout`. It ensures `config.template_dir`
/// exists, then runs `svn co <path>/<rrid>` with `cwd = template_dir` (the URI
/// is absolute, so we set `cwd` rather than mutating the process-global working
/// directory). `svn`'s stderr is captured so its cryptic `E170000: URL ...
/// doesn't exist` line is surfaced only at debug; the caller sees a clear,
/// actionable [`CheckoutError::SvnCheckoutFailed`] pointing at the log URL.
///
/// # Errors
///
/// - [`CheckoutError::TemplateDirNotUsable`] if `template_dir` cannot be created
///   (a plain file in the way, permission denied, …).
/// - [`CheckoutError::SvnCheckoutFailed`] if `svn co` exits non-zero (e.g. the
///   report does not exist).
pub async fn testreport_svn_checkout(
    runner: &dyn SvnRunner,
    config: &Config,
    path: &str,
    rrid: &RequestReviewID,
) -> Result<(), CheckoutError> {
    let template_dir = &config.template_dir;

    // A template_dir that cannot be created (permission denied, a plain file in
    // the way, …) would otherwise escape as a raw I/O error; surface it as one
    // clear error naming the option (upstream `ensure_dir_exists` +
    // `TemplateDirNotUsableError`).
    if let Err(e) = std::fs::create_dir_all(template_dir) {
        return Err(CheckoutError::TemplateDirNotUsable {
            path: template_dir.display().to_string(),
            reason: e.to_string(),
        });
    }

    // `os.path.join(path, str(rrid))` — the base repo path is a URI, so a plain
    // string join (not a filesystem `Path` join) matches upstream byte-for-byte.
    let uri = format!("{}/{}", path.trim_end_matches('/'), rrid);

    let outcome = runner
        .run(&["co".to_owned(), uri.clone()], template_dir)
        .await
        .map_err(|e| {
            // A spawn/await failure (e.g. `svn` not installed) is a failed
            // checkout from the caller's perspective.
            debug!("svn co {uri} could not run: {e}");
            svn_checkout_failed(config, rrid)
        })?;

    if !outcome.success {
        if !outcome.stderr.is_empty() {
            debug!("svn co {uri} failed: {}", outcome.stderr.trim());
        }
        return Err(svn_checkout_failed(config, rrid));
    }

    Ok(())
}

/// Builds the [`CheckoutError::SvnCheckoutFailed`] for `rrid`, deriving the log
/// URL from `config.fancy_reports_url` (upstream
/// `f"{fancy_reports_url.rstrip('/')}/{rrid}/log"`).
fn svn_checkout_failed(config: &Config, rrid: &RequestReviewID) -> CheckoutError {
    let report_url = format!(
        "{}/{}/log",
        config.fancy_reports_url.trim_end_matches('/'),
        rrid
    );
    CheckoutError::SvnCheckoutFailed {
        rrid: rrid.to_string(),
        report_url,
    }
}

/// Adds the testreport artifacts to SVN and commits the working copy.
///
/// Ports upstream `svn_commit_testreport` — the reusable core of the `commit`
/// command, shared so other commands (e.g. `approve -r`) can commit the
/// testreport too. The argv sequence is reproduced exactly:
///
/// 1. `svn add --force <install_logs>`
/// 2. `svn add --force results` — only if a `results` dir exists
/// 3. `svn add --force checkers.log` — only if a `checkers.log` file exists
/// 4. `svn up`
/// 5. `svn ci [msg...]`
///
/// # Errors
///
/// Returns [`CheckoutError::SvnCheckoutFailed`] if a required `svn` call fails,
/// so callers can decide whether to abort (upstream lets the `subprocess`
/// exception propagate).
pub async fn svn_commit_testreport(
    runner: &dyn SvnRunner,
    checkout: &Path,
    install_logs: &Path,
    msg: &[String],
) -> Result<(), CheckoutError> {
    run_checked(
        runner,
        &[
            "add".to_owned(),
            "--force".to_owned(),
            install_logs.display().to_string(),
        ],
        checkout,
    )
    .await?;

    // Upstream uses `subprocess.call` (unchecked) for the `results` add; a
    // missing/awkward `results` dir must not abort the commit.
    if checkout.join("results").exists() {
        let _ = runner
            .run(
                &["add".to_owned(), "--force".to_owned(), "results".to_owned()],
                checkout,
            )
            .await;
    }

    if checkout.join("checkers.log").exists() {
        run_checked(
            runner,
            &[
                "add".to_owned(),
                "--force".to_owned(),
                "checkers.log".to_owned(),
            ],
            checkout,
        )
        .await?;
    }

    run_checked(runner, &["up".to_owned()], checkout).await?;

    let mut ci = vec!["ci".to_owned()];
    ci.extend_from_slice(msg);
    run_checked(runner, &ci, checkout).await?;

    Ok(())
}

/// Runs a `svn` subcommand, turning a spawn failure or non-zero exit into a
/// [`CheckoutError`] (upstream `subprocess.check_call`).
async fn run_checked(
    runner: &dyn SvnRunner,
    args: &[String],
    cwd: &Path,
) -> Result<(), CheckoutError> {
    let outcome = runner.run(args, cwd).await.map_err(|e| {
        debug!("svn {} could not run: {e}", args.join(" "));
        commit_failed()
    })?;
    if !outcome.success {
        if !outcome.stderr.is_empty() {
            debug!("svn {} failed: {}", args.join(" "), outcome.stderr.trim());
        }
        return Err(commit_failed());
    }
    Ok(())
}

/// A generic commit failure. The commit path has no RRID/log-URL context, so it
/// reuses [`CheckoutError::SvnCheckoutFailed`] with empty context — the caller
/// (the `commit` command wrapper, a later task) logs the actionable detail.
fn commit_failed() -> CheckoutError {
    CheckoutError::SvnCheckoutFailed {
        rrid: String::new(),
        report_url: String::new(),
    }
}
