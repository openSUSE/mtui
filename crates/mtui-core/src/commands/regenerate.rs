//! The `regenerate` command — regenerate the loaded update's template.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_testreport::UpdateKind;
use mtui_types::{UpdateID, Workflow};
use tracing::info;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
use crate::commands::support::{require_update, template_completion};
use crate::error::CommandResult;
use crate::session::Session;

/// Regenerates the loaded update's test-report template via the TeReGen API.
///
/// Ports upstream `mtui.commands.regenerate.Regenerate`. Enqueues a regeneration
/// job (`POST /reports/{id}/regenerate`); by default waits for the Minion job to
/// finish and reports the outcome. `--force` overwrites an existing but unedited
/// template, `--ignore-inconsistent` regenerates despite inconsistent metadata,
/// and `--no-wait` enqueues and returns immediately.
///
/// After a successful wait, the freshly built template is **reloaded** (upstream
/// `_reload`): the stale local checkout is dropped and the update is re-loaded
/// via [`Session::load_update`] without autoconnect, so the new build is picked
/// up in place without leaving mtui.
pub struct Regenerate;

#[async_trait]
impl Command for Regenerate {
    fn name(&self) -> &'static str {
        "regenerate"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Regenerates the loaded update's test-report template via the TeReGen API.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn mutates_registry(&self) -> bool {
        true
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .help("overwrite an existing (but unedited) template"),
        )
        .arg(
            Arg::new("ignore_inconsistent")
                .long("ignore-inconsistent")
                .action(ArgAction::SetTrue)
                .help("regenerate despite inconsistent metadata (e.g. arch mismatch)"),
        )
        .arg(
            Arg::new("no_wait")
                .long("no-wait")
                .action(ArgAction::SetTrue)
                .help("enqueue the job and return without waiting or reloading"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = ["--force", "--ignore-inconsistent", "--no-wait"]
            .iter()
            .filter(|f| f.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let force = args.get_flag("force");
        let ignore_inconsistent = args.get_flag("ignore_inconsistent");
        let no_wait = args.get_flag("no_wait");

        let teregen = teregen_client(session)?;
        let rrid_str = rrid.to_string();

        if no_wait {
            let result = teregen
                .regenerate(&rrid_str, force, ignore_inconsistent)
                .await;
            report_enqueue(
                session,
                &rrid_str,
                result.as_ref(),
                force,
                ignore_inconsistent,
            );
            return Ok(());
        }

        // Drive a TTY spinner for the (long-polling) wait — upstream's
        // `with spinner(f"Regenerating {rrid}")`. REPL-only (gated on
        // `interactive`, like the fan-out spinner); a no-op off a TTY / over
        // MCP. The guard's `is_stopped` predicate feeds `regenerate_and_wait`'s
        // cooperative-cancel hook so Ctrl-C during the wait bails out promptly
        // instead of blocking to the next poll.
        let spin = session
            .is_repl
            .then(|| mtui_hosts::spinner(format!("Regenerating {rrid_str}")));
        let should_stop = || {
            spin.as_ref()
                .is_some_and(mtui_hosts::SpinnerGuard::is_stopped)
        };
        let outcome = teregen
            .regenerate_and_wait(&rrid_str, force, ignore_inconsistent, should_stop)
            .await;
        drop(spin);

        if outcome.unreachable {
            session.display.println(&format!(
                "Regeneration request for {rrid_str} failed (TeReGen unreachable)"
            ));
            return Ok(());
        }
        if let Some(error) = &outcome.error {
            session
                .display
                .println(&format!("Regeneration refused: {error}"));
            println_retry_hint(session, force, ignore_inconsistent);
            return Ok(());
        }
        if !outcome.ok {
            let state = outcome.state.as_deref().unwrap_or("unknown");
            let mut msg = format!("Regeneration of {rrid_str} did not finish (state: {state})");
            if let Some(err) = &outcome.minion_error {
                msg.push_str(&format!(": {err}"));
            }
            session.display.println(&msg);
            return Ok(());
        }

        // Success. Drop the stale checkout and reload the freshly built template
        // in place (upstream `_reload`).
        session
            .display
            .println(&format!("Template for {rrid_str} regenerated — reloading"));
        reload(session, &rrid_str).await;
        Ok(())
    }
}

/// Drops the stale local checkout and reloads the freshly built template
/// (upstream `Regenerate._reload`).
///
/// Reconstructs the update kind from the active report's workflow
/// ([`Workflow::Kernel`] → [`UpdateKind::Kernel`], else [`UpdateKind::Auto`],
/// mirroring the upstream `KernelOBSUpdateID`/`AutoOBSUpdateID` factory choice),
/// removes `template_dir/<rrid>` best-effort, then re-loads the update **without**
/// autoconnect (no live-host grab on a regen-reload).
async fn reload(session: &mut Session, rrid: &str) {
    let kind = match session.metadata().workflow() {
        Workflow::Kernel => UpdateKind::Kernel,
        _ => UpdateKind::Auto,
    };

    // Drop the stale checkout so the reload re-checks-out the new build. Upstream
    // uses `shutil.rmtree(..., ignore_errors=True)`: a missing/undeletable dir is
    // not fatal.
    let trdir = session.config.template_dir.join(rrid);
    if trdir.exists() {
        match tokio::fs::remove_dir_all(&trdir).await {
            Ok(()) => info!("Removed stale checked out template {}", trdir.display()),
            Err(e) => info!(
                "Could not remove stale template {}: {e} (continuing)",
                trdir.display()
            ),
        }
    }

    // The RRID came from a loaded report, so it parses; if it somehow does not,
    // skip the reload rather than abort the command.
    let update = match UpdateID::parse(rrid) {
        Ok(u) => u,
        Err(e) => {
            info!("Skipping reload of {rrid}: could not parse RRID: {e}");
            return;
        }
    };

    session.load_update(&update, false, kind).await;
}

/// Reports the `--no-wait` enqueue outcome (upstream `_report_enqueue`).
fn report_enqueue(
    session: &mut Session,
    rrid: &str,
    result: Option<&serde_json::Value>,
    force: bool,
    ignore_inconsistent: bool,
) {
    let Some(result) = result else {
        session.display.println(&format!(
            "Regeneration request for {rrid} failed (TeReGen unreachable)"
        ));
        return;
    };
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        session
            .display
            .println(&format!("Regeneration refused: {error}"));
        println_retry_hint(session, force, ignore_inconsistent);
        return;
    }
    let job = result
        .get("job")
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| "?".to_owned());
    session
        .display
        .println(&format!("Regeneration job {job} enqueued for {rrid}"));
    session
        .display
        .println("Not waiting (--no-wait); reload the template once it is built.");
}

/// Suggests the flags that might lift a refusal, skipping ones already set
/// (upstream `_println_retry_hint`).
fn println_retry_hint(session: &mut Session, force: bool, ignore_inconsistent: bool) {
    let mut flags = Vec::new();
    if !force {
        flags.push("--force");
    }
    if !ignore_inconsistent {
        flags.push("--ignore-inconsistent");
    }
    if !flags.is_empty() {
        session.display.println(&format!(
            "Retry with {} if appropriate.",
            flags.join(" and/or ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;
    use mtui_config::Config;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Regenerate.name(), "regenerate");
        assert_eq!(Regenerate.scope(), Scope::Fanout);
    }

    #[test]
    fn completion_offers_flags() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let mut out = Regenerate.complete(&session, "--", "");
        out.retain(|c| c.starts_with("--"));
        assert!(out.contains(&"--force".to_owned()));
        assert!(out.contains(&"--no-wait".to_owned()));
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Regenerate, &[]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    fn config_for(server: &MockServer) -> Config {
        let mut c = Config::default();
        c.teregen_api = server.uri();
        c
    }

    #[tokio::test]
    async fn no_wait_reports_enqueued_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reports/SUSE:Maintenance:1:1/regenerate"))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 77})))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config = config_for(&server);
        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Regeneration job 77 enqueued"), "{out}");
        assert!(out.contains("Not waiting"), "{out}");
    }

    #[tokio::test]
    async fn no_wait_reports_refusal_with_retry_hint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reports/SUSE:Maintenance:1:1/regenerate"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error": "template exists"})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config = config_for(&server);
        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("Regeneration refused: template exists"),
            "{out}"
        );
        assert!(out.contains("--force"), "{out}");
    }

    /// Mounts the success mocks (regenerate → 202, status → finished) on `server`.
    async fn mount_success(server: &MockServer, rrid: &str) {
        Mock::given(method("POST"))
            .and(path(format!("/reports/{rrid}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 5})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{rrid}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"minion_state": "finished"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn success_reloads_and_drops_stale_checkout() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;

        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        let tmp = tempfile::tempdir().unwrap();
        // Seed a stale checkout with a marker file; the reload must remove it.
        let trdir = tmp.path().join(rrid);
        std::fs::create_dir_all(&trdir).unwrap();
        let marker = trdir.join("stale-marker");
        std::fs::write(&marker, "old\n").unwrap();

        session.config = config_for(&server);
        session.config.template_dir = tmp.path().to_path_buf();
        // Offline svn: the post-removal re-checkout yields a NullReport, so the
        // reload degrades gracefully without touching the network.
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &[]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
        // The stale checkout (and its marker) were dropped by the reload leg.
        assert!(!marker.exists(), "stale checkout should have been removed");
        assert!(
            !trdir.exists(),
            "stale checkout dir should have been removed"
        );
    }

    #[tokio::test]
    async fn success_reload_does_not_autoconnect() {
        // Kernel workflow selects UpdateKind::Kernel, which never autoconnects;
        // more generally the reload passes autoconnect=false, so no hosts are
        // connected as a side effect of reloading.
        let rrid = "SUSE:Maintenance:2:2";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;

        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Kernel;
        let tmp = tempfile::tempdir().unwrap();
        session.config = config_for(&server);
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let before = session.targets().len();
        let args = matches(&Regenerate, &[]);
        Regenerate.call(&mut session, &args).await.unwrap();

        // The reload passes autoconnect=false: it never grabs additional pool
        // hosts, so the target count is unchanged by the regen-reload.
        assert_eq!(session.targets().len(), before);
    }

    #[tokio::test]
    async fn unreachable_teregen_reports_cleanly() {
        // Point at a closed port so the POST fails at the transport layer.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let mut c = Config::default();
        c.teregen_api = "http://127.0.0.1:1/api".to_owned();
        session.config = c;
        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("TeReGen unreachable"),
            "{}",
            buf.contents()
        );
    }
}
