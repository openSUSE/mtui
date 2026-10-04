//! The `regenerate` command — regenerate the loaded update's template.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::teregen::{DocumentFetch, TeregenV2, TeregenV2Error};
use mtui_testreport::UpdateKind;
use mtui_types::report_document::ReportDocument;
use mtui_types::{UpdateID, Workflow};
use tracing::info;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
use crate::commands::support::{document_edits_guard, require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Regenerates a test-report template via the TeReGen API.
///
/// Enqueues `POST /reports/{id}/regenerate` and, by default, waits for the Minion
/// job. `--force` overwrites an existing but unedited template,
/// `--ignore-inconsistent` regenerates despite inconsistent metadata, `--no-wait`
/// enqueues and returns.
///
/// The optional `RRID` positional is regenerated *without* being loaded first,
/// breaking the load/regenerate catch-22 for a never-generated report (a missing
/// SLFO template cannot be loaded, and TeReGen is what creates it); omitted,
/// the active template is used.
///
/// After a successful wait the freshly built template is loaded in place via
/// [`Session::load_update`] without autoconnect. The kind is inferred from the
/// loaded report when one exists; for a standalone RRID it is
/// [`UpdateKind::Auto`], or [`UpdateKind::Kernel`] with `-k/--kernel`. The
/// regenerated RRID becomes the active template afterwards unconditionally —
/// even one already loaded but inactive, or with a different template
/// active — announced in the command's output when it moves the pointer.
///
/// It names its own target and never fans out ([`Scope::Single`]).
pub struct Regenerate;

#[async_trait]
impl Command for Regenerate {
    fn name(&self) -> &'static str {
        "regenerate"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Regenerates a test-report template via the TeReGen API \
             (a given RRID need not be loaded first).",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn requires_canonical_session(&self, _argv: &[String]) -> bool {
        true
    }

    /// The regenerated RRID always becomes active unconditionally — not only a
    /// standalone one — so `call` announces the move when it changes the
    /// pointer.
    fn repoints_active(&self) -> bool {
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
        .arg(
            Arg::new("kernel")
                .short('k')
                .long("kernel")
                .action(ArgAction::SetTrue)
                .help("load the standalone RRID as a kernel update (default: auto)"),
        )
        .arg(
            Arg::new("discard_authored")
                .long("discard-authored")
                .action(ArgAction::SetTrue)
                .help(
                    "override mtui's own guards against regenerating a document that already \
                     carries tester content (verdict, testers, issue answers, install/regression \
                     results), loaded or on the server, or holds edits that were never \
                     committed; without it the REPL asks before discarding such content and \
                     MCP refuses; teregen itself still refuses on a verdict or testers",
                ),
        )
        .arg(
            Arg::new("rrid")
                .value_name("RRID")
                .help("template to regenerate even if not loaded (default: the loaded template)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = [
            "--force",
            "--ignore-inconsistent",
            "--no-wait",
            "-k",
            "--discard-authored",
        ]
        .iter()
        .filter(|f| f.starts_with(text))
        .map(|s| (*s).to_owned())
        .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let force = args.get_flag("force");
        let ignore_inconsistent = args.get_flag("ignore_inconsistent");
        let no_wait = args.get_flag("no_wait");
        let kernel = args.get_flag("kernel");
        let discard_authored = args.get_flag("discard_authored");

        // Only the fallback goes through the "load first" guard; an explicit
        // RRID breaks the load/regenerate catch-22.
        let rrid_str = match args.get_one::<String>("rrid") {
            Some(rrid) => rrid.clone(),
            None => require_update(session)?.to_string(),
        };

        // Registry-wide: also covers a loaded-but-inactive RRID, and the edits
        // `has_tester_content` misses (`testing.openqa`-only authoring). Local
        // only, so it runs before the gate's server read.
        document_edits_guard(session, &rrid_str, discard_authored)?;

        tester_content_gate(session, &rrid_str, discard_authored).await?;

        let teregen = teregen_client(session)?;

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

        // REPL-only, like the fan-out spinner; a no-op off a TTY / over MCP.
        let spin = session
            .is_repl
            .then(|| mtui_hosts::spinner(format!("Regenerating {rrid_str}")));
        // Cooperative-cancel hook off the session seam, so a Ctrl-C or an MCP
        // `job_cancel` abandons the wait at its next step, not the next poll.
        // The spinner's own stop flag stays in the predicate: the display layer
        // sets it and a cancel never does, so neither covers for the other.
        let cancel = session.cancel_token();
        let should_stop = || {
            cancel.is_cancelled()
                || spin
                    .as_ref()
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

        session
            .display
            .println(&format!("Template for {rrid_str} regenerated — reloading"));
        let previously_active = session.templates.active_rrid().map(str::to_owned);
        reload(session, &rrid_str, kernel).await;
        // `repoints_active` leaves the reloaded RRID active unconditionally, so
        // announce it whenever that actually moved the pointer away from what
        // was active before — the reload's own output otherwise gives no hint.
        if session.templates.active_rrid() == Some(rrid_str.as_str())
            && previously_active.as_deref() != Some(rrid_str.as_str())
        {
            session
                .display
                .println(&format!("{rrid_str} is now the active template"));
        }
        Ok(())
    }
}

/// Stops `regenerate` from wiping tester-authored content unless the operator
/// accepts the loss: the loaded document is checked first, then the server's
/// copy (an anonymous `GET`, so a report that is not loaded is covered too).
/// Content found is a `[y/N]` question in the REPL with a prompter and a
/// refusal anywhere else, as in `approve`'s hash gate.
///
/// `404`/`409` mean there is nothing to lose. Any other failure to read the
/// server's copy refuses: an unchecked document must not pass as clean.
/// `discard_authored` skips the check, and the `GET` with it.
async fn tester_content_gate(
    session: &mut Session,
    rrid: &str,
    discard_authored: bool,
) -> Result<(), CommandError> {
    if discard_authored {
        return Ok(());
    }
    let local = session
        .metadata()
        .rrid()
        .is_some_and(|r| r.to_string() == rrid)
        && session
            .metadata()
            .base()
            .document
            .as_ref()
            .is_some_and(ReportDocument::has_tester_content);
    if !local {
        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
        let client = TeregenV2::with_client(http, &session.config.teregen_api_v2);
        if !server_has_tester_content(&client, rrid).await? {
            return Ok(());
        }
    }

    let (place, advice) = if local {
        ("loaded document", "run `commit` first, or pass")
    } else {
        ("document on the server", "pass")
    };
    if session.is_repl
        && let Some(prompter) = session.prompter()
    {
        let confirmed = prompter
            .confirm(
                &format!(
                    "{rrid}'s {place} carries tester-authored content; regenerate and discard \
                     it? [y/N]: "
                ),
                false,
            )
            .await;
        return if confirmed {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "not regenerating {rrid}: its {place} carries tester-authored content"
            )))
        };
    }
    Err(CommandError::Other(format!(
        "{rrid}'s {place} already carries tester-authored content (a verdict, testers, issue \
         answers, or install/regression results); {advice} --discard-authored to regenerate \
         anyway — teregen itself still refuses on a verdict or testers"
    )))
}

async fn server_has_tester_content(client: &TeregenV2, rrid: &str) -> Result<bool, CommandError> {
    match client.fetch_document(rrid, None).await {
        Ok(DocumentFetch::Fresh { document, .. }) => Ok(document.has_tester_content()),
        Ok(DocumentFetch::NotModified) | Err(TeregenV2Error::NotFound | TeregenV2Error::Stale) => {
            Ok(false)
        }
        Err(e) => Err(CommandError::Other(format!(
            "cannot check {rrid}'s document on the server for tester-authored content: {e}; \
             pass --discard-authored to regenerate anyway"
        ))),
    }
}

/// Drops any stale local checkout (`template_dir/<rrid>`, best-effort) and loads
/// the freshly built template **without** autoconnect — no live-host grab on a
/// regen-reload. The kind comes from a loaded report whose RRID matches `rrid`
/// ([`Workflow::Kernel`] → [`UpdateKind::Kernel`], else [`UpdateKind::Auto`]),
/// falling back to `kernel_hint` for a standalone RRID with no workflow to read.
async fn reload(session: &mut Session, rrid: &str, kernel_hint: bool) {
    let loaded_matches = session
        .metadata()
        .rrid()
        .is_some_and(|r| r.to_string() == rrid);
    let kind = if loaded_matches {
        match session.metadata().workflow() {
            Workflow::Kernel => UpdateKind::Kernel,
            _ => UpdateKind::Auto,
        }
    } else if kernel_hint {
        UpdateKind::Kernel
    } else {
        UpdateKind::Auto
    };

    // Drop the stale checkout so the reload re-checks-out the new build.
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

    // Skip the reload rather than abort the whole command on an unparseable RRID.
    let update = match UpdateID::parse(rrid) {
        Ok(u) => u,
        Err(e) => {
            info!("Skipping reload of {rrid}: could not parse RRID: {e}");
            return;
        }
    };

    // `false`: reloading a template TeReGen just regenerated, not one it refused.
    session.load_update(&update, false, kind, false).await;
}

/// Reports the `--no-wait` enqueue outcome.
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

/// Suggests the flags that might lift a refusal, skipping ones already set.
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
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};
    use crate::error::CommandError;
    use mtui_config::Config;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Regenerate.name(), "regenerate");
        // It names its own target, so it must never fan out across siblings.
        assert_eq!(Regenerate.scope(), Scope::Single);
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
        c.teregen_api_v2 = server.uri();
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
        // Offline svn: the re-checkout yields a NullReport, no network touched.
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &[]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
        assert!(!marker.exists(), "stale checkout should have been removed");
        assert!(
            !trdir.exists(),
            "stale checkout dir should have been removed"
        );
        assert!(
            !out.contains("is now the active template"),
            "regenerating the already-active template must not announce a move: {out}"
        );
    }

    #[tokio::test]
    async fn success_reload_does_not_autoconnect() {
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

        // autoconnect=false: the regen-reload grabs no additional pool hosts.
        assert_eq!(session.targets().len(), before);
    }

    /// Signals the test the first time the mocked endpoint is hit, so a cancel
    /// can be timed to land *during* the wait rather than before it.
    struct SignalOnFirstHit {
        hit: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        body: serde_json::Value,
    }

    impl wiremock::Respond for SignalOnFirstHit {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            if let Some(tx) = self.hit.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ResponseTemplate::new(200).set_body_json(self.body.clone())
        }
    }

    /// A cancel arriving **mid-wait** abandons the long-polling wait promptly.
    ///
    /// Cancelling *before* the call would not prove it: a predicate reading the
    /// token once at closure-build time would pass that way and still leave a
    /// live Ctrl-C waiting out the poll interval. So the cancel fires only after
    /// one TeReGen poll, with the wait inside its inter-poll sleep. The timeout
    /// is the assertion: the job never finishes, so a wait that stops observing
    /// the seam sleeps the full 5s poll interval, repeatedly.
    #[tokio::test]
    async fn a_cancel_mid_wait_abandons_it_promptly() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{rrid}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 9})))
            .mount(&server)
            .await;
        // Still running, forever: only the cancel can end this wait.
        let (hit_tx, hit_rx) = tokio::sync::oneshot::channel();
        Mock::given(method("GET"))
            .and(path(format!("/reports/{rrid}/status")))
            .respond_with(SignalOnFirstHit {
                hit: std::sync::Mutex::new(Some(hit_tx)),
                body: serde_json::json!({"minion_state": "running"}),
            })
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        let cancel = session.cancel_token();
        tokio::spawn(async move {
            hit_rx
                .await
                .expect("the wait must poll TeReGen at least once");
            cancel.cancel();
        });

        let args = matches(&Regenerate, &[]);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Regenerate.call(&mut session, &args),
        )
        .await
        .expect("the wait must observe the cancel, not poll on")
        .unwrap();

        // It reports what it saw rather than claiming success or failure.
        let out = buf.contents();
        assert!(out.contains("did not finish (state: running)"), "{out}");
    }

    #[tokio::test]
    async fn unreachable_teregen_reports_cleanly() {
        // Point at a closed port so the POST fails at the transport layer.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let mut c = Config::default();
        c.teregen_api = "http://127.0.0.1:1/api".to_owned();
        session.config = c;
        let args = matches(&Regenerate, &["--no-wait", "--discard-authored"]);
        Regenerate.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("TeReGen unreachable"),
            "{}",
            buf.contents()
        );
    }

    /// `regenerate <RRID>` on an empty session reaches the TeReGen POST (the
    /// finished status only comes back if it landed) instead of erroring
    /// `Metadata not loaded`. The auto workflow's Gitea hash-check needs a token
    /// this offline test can't satisfy, so the load degrades to a NullReport —
    /// registration on success is proven by the kernel test below.
    #[tokio::test]
    async fn standalone_rrid_regenerates_without_load_first() {
        let rrid = "SUSE:SLFO:1.2:6311";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;

        let (mut session, buf) = empty_session();
        let tmp = tempfile::tempdir().unwrap();
        session.config = config_for(&server);
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &[rrid]);
        // Crucially: no `require_update` error despite nothing being loaded.
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
    }

    /// `regenerate` is `Scope::Single` but reads the report it was handed
    /// (`require_update`), so the busy refusal applies: off the null sentinel a
    /// bare `regenerate` reported "no update loaded" over a template that very
    /// much is (#524).
    #[tokio::test]
    async fn contended_template_is_refused() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.release_active_guard();
        let entry = session
            .templates
            .handle("SUSE:Maintenance:1:1")
            .expect("seeded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let args = matches(&Regenerate, &[]);
        let err = Regenerate
            .run(&mut session, &args)
            .await
            .expect_err("a report-reading Single-scope command must refuse a held entry");
        assert!(
            matches!(err, CommandError::TemplateBusy(ref r) if r == "SUSE:Maintenance:1:1"),
            "got: {err:?}"
        );
    }

    /// A standalone `-k <RRID>` loads with the kernel workflow, proving the
    /// success path registers the RRID (`load_update`'s registration is
    /// kind-agnostic). Also drives `Command::run` (not `call`) with a different
    /// template already active: the regenerated RRID must end up active, and a
    /// reverted pointer would fail the `workflow() == Kernel` assert below too
    /// (the restored template carries no workflow). Skipped where `svn` is
    /// absent — it still runs on CI's Ubuntu leg, which is where this
    /// regression would otherwise resurface unnoticed.
    #[tokio::test]
    async fn standalone_rrid_kernel_hint_loads_kernel_workflow() {
        // `reload` deletes `template_dir/<rrid>`, so the report must come back
        // from SVN. A local `file://` repo (never the `qam.suse.de` default)
        // keeps that hermetic, and skips cleanly where `svn` is absent.
        if std::process::Command::new("svn")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // svn not installed in this environment
        }

        let rrid = "SUSE:Maintenance:24993:275518";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;
        crate::commands::testkit::teregen::mount_document(
            &server,
            rrid,
            &crate::commands::testkit::teregen::document_json(rrid, "maintenance"),
        )
        .await;
        crate::commands::testkit::teregen::mount_schema(&server).await;

        let (mut session, buf) = empty_session();
        // A different template is already active; the regenerated RRID must
        // still end up active (openSUSE/mtui#564).
        session
            .templates
            .add(fake_report("SUSE:Maintenance:1:1", &["h1"], "ok"));
        assert!(session.activate("SUSE:Maintenance:1:1").is_active());
        let tmp = tempfile::tempdir().unwrap();

        let repo = tmp.path().join("repo");
        assert!(
            std::process::Command::new("svnadmin")
                .args(["create", repo.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let repo_url = format!("file://{}", repo.display());
        let import = tmp.path().join("import").join(rrid);
        std::fs::create_dir_all(&import).unwrap();
        std::fs::write(import.join("log"), "log\n").unwrap();
        std::fs::write(
            import.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();
        assert!(
            std::process::Command::new("svn")
                .args([
                    "import",
                    "-m",
                    "seed",
                    import.parent().unwrap().to_str().unwrap(),
                    &repo_url,
                ])
                .status()
                .unwrap()
                .success()
        );

        session.config = config_for(&server);
        session.config.template_dir = tmp.path().join("templates");
        session.config.svn_path = repo_url;

        let args = matches(&Regenerate, &["-k", rrid]);
        Regenerate.run(&mut session, &args).await.unwrap();

        assert!(
            session.templates.contains(rrid),
            "RRID should be registered"
        );
        assert_eq!(session.metadata().workflow(), Workflow::Kernel);
        assert_eq!(
            session.templates.active_rrid(),
            Some(rrid),
            "the regenerated RRID must end up active, even with another already active"
        );
        assert!(
            buf.contents()
                .contains(&format!("{rrid} is now the active template")),
            "moving the pointer away from the prior active template must be \
             announced: {:?}",
            buf.contents()
        );
    }

    /// `--no-wait` with an explicit RRID on an empty session enqueues and
    /// returns without loading anything.
    #[tokio::test]
    async fn standalone_rrid_no_wait_enqueues_without_load() {
        let rrid = "SUSE:SLFO:1.2:6311";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{rrid}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 9})))
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        session.config = config_for(&server);
        let args = matches(&Regenerate, &["--no-wait", rrid]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("Regeneration job 9 enqueued"), "{out}");
        assert!(out.contains("Not waiting"), "{out}");
        assert!(
            !session.templates.contains(rrid),
            "--no-wait must not load the template"
        );
    }

    #[test]
    fn accepts_optional_rrid_and_kernel_hint() {
        let cmd = Regenerate.configure(clap::Command::new("regenerate").no_binary_name(true));
        assert!(cmd.clone().try_get_matches_from([] as [&str; 0]).is_ok());
        assert!(
            cmd.clone()
                .try_get_matches_from(["SUSE:SLFO:1.2:6311"])
                .is_ok()
        );
        assert!(
            cmd.try_get_matches_from(["-k", "SUSE:Maintenance:1:1"])
                .is_ok()
        );
    }

    // --- --discard-authored guard ---

    /// A schema-valid document for `id`, with `testing` set to `extra_testing`
    /// verbatim (a JSON object literal).
    fn document_json(id: &str, extra_testing: &str) -> String {
        format!(
            r#"{{
                "schema_version": "1.0", "id": "{id}", "kind": "pi",
                "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
                "verdict": null, "comment": null,
                "people": {{"testers": [], "reviewer": {{"name": null}}}},
                "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                           "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}],
                           "patches": [{{"id": "1", "title": "t"}}]}},
                "install": {{"repository": "http://x/", "targets": [{{
                    "product": "n", "version": "v", "arch": "x86_64",
                    "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
                }}], "test_platforms": []}},
                "issues": {{}}, "testing": {extra_testing}
            }}"#
        )
    }

    fn doc_with_verdict(id: &str) -> ReportDocument {
        document_json(id, "{}")
            .replace("\"verdict\": null", "\"verdict\": \"PASSED\"")
            .parse()
            .unwrap()
    }

    fn doc_with_openqa_only(id: &str) -> ReportDocument {
        document_json(id, r#"{"openqa": {}}"#).parse().unwrap()
    }

    #[tokio::test]
    async fn discard_authored_refuses_regenerating_loaded_tester_content_without_the_flag() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        // Deliberately no mocks mounted: any request is a hard failure.
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.metadata_mut().base_mut().document = Some(doc_with_verdict(rrid));

        let args = matches(&Regenerate, &[]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--discard-authored")),
            "{err:?}"
        );

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "the guard must send nothing: {requests:?}"
        );
    }

    #[tokio::test]
    async fn discard_authored_flag_proceeds_past_the_guard() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;

        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.metadata_mut().base_mut().document = Some(doc_with_verdict(rrid));
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &["--discard-authored"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
    }

    #[tokio::test]
    async fn dirty_openqa_only_document_is_refused_without_any_request() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        let base = session.metadata_mut().base_mut();
        base.document = Some(doc_with_openqa_only(rrid));
        base.document_dirty = true;

        let args = matches(&Regenerate, &[]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--discard-authored")),
            "{err:?}"
        );
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }

    #[tokio::test]
    async fn discard_authored_lifts_the_dirty_guard() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;
        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        let base = session.metadata_mut().base_mut();
        base.document = Some(doc_with_openqa_only(rrid));
        base.document_dirty = true;
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &["--discard-authored"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert!(buf.contents().contains("regenerated — reloading"));
    }

    /// The dirty guard is registry-wide: a loaded RRID that is not the active
    /// one is covered too.
    #[tokio::test]
    async fn a_dirty_inactive_template_is_refused_when_named() {
        let server = MockServer::start().await;
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config = config_for(&server);
        let mut other = mtui_testreport::TestReportBase::new(Config::default());
        other.rrid = "SUSE:Maintenance:2:2".parse().ok();
        other.document_dirty = true;
        session
            .templates
            .add(crate::commands::testkit::fake_report_from_base(other));

        let args = matches(&Regenerate, &["SUSE:Maintenance:2:2"]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--discard-authored")),
            "{err:?}"
        );
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }

    /// The guard only ever consults the *loaded* report's document for the
    /// RRID it is actually loaded under — an explicit RRID naming a different,
    /// standalone update is never refused, even while the loaded report
    /// itself carries tester content.
    #[tokio::test]
    async fn explicit_rrid_different_from_the_loaded_one_is_never_refused() {
        let loaded_rrid = "SUSE:Maintenance:1:1";
        let other_rrid = "SUSE:SLFO:1.2:6311";
        let server = MockServer::start().await;
        mount_success(&server, other_rrid).await;

        let (mut session, buf) = session_with_hosts(loaded_rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.metadata_mut().base_mut().document = Some(doc_with_verdict(loaded_rrid));
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &[other_rrid]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
    }

    /// `testing.openqa` alone is never evidence of tester content
    /// (`ReportDocument::has_tester_content`), so the guard must not fire.
    #[tokio::test]
    async fn openqa_only_document_is_not_refused() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_success(&server, rrid).await;

        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.metadata_mut().base_mut().document = Some(doc_with_openqa_only(rrid));
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&Regenerate, &[]);
        Regenerate.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("regenerated — reloading"), "{out}");
    }

    // --- server-side tester-content gate ---

    fn doc_with_issue_status(id: &str) -> String {
        document_json(id, "{}").replace(
            "\"issues\": {}",
            "\"issues\": {\"bsc#1\": {\"title\": \"t\", \"reproducer\": null, \
             \"status\": \"FIXED\", \"comment\": null}}",
        )
    }

    async fn mount_document(server: &MockServer, rrid: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/reports/{rrid}")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn mount_enqueue(server: &MockServer, rrid: &str) {
        Mock::given(method("POST"))
            .and(path(format!("/reports/{rrid}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 3})))
            .mount(server)
            .await;
    }

    async fn count(server: &MockServer, verb: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == verb)
            .count()
    }

    fn fixed_prompter(answer: &'static str) -> mtui_hosts::Prompter {
        mtui_hosts::Prompter::new(std::sync::Arc::new(move |_t: String| {
            Box::pin(async move { Ok(answer.to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }))
    }

    #[tokio::test]
    async fn server_content_refuses_without_a_prompter_and_enqueues_nothing() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(
            &server,
            rrid,
            ResponseTemplate::new(200).set_body_string(doc_with_issue_status(rrid)),
        )
        .await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait"]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("on the server") && m.contains("--discard-authored")),
            "{err:?}"
        );
        assert_eq!(count(&server, "POST").await, 0);
    }

    #[tokio::test]
    async fn repl_declined_prompt_refuses_and_enqueues_nothing() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(
            &server,
            rrid,
            ResponseTemplate::new(200).set_body_string(doc_with_issue_status(rrid)),
        )
        .await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.is_repl = true;
        session.set_prompter(fixed_prompter("n"));

        let args = matches(&Regenerate, &["--no-wait"]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("not regenerating")),
            "{err:?}"
        );
        assert_eq!(count(&server, "POST").await, 0);
    }

    #[tokio::test]
    async fn repl_confirmed_prompt_enqueues_once() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(
            &server,
            rrid,
            ResponseTemplate::new(200).set_body_string(doc_with_issue_status(rrid)),
        )
        .await;
        mount_enqueue(&server, rrid).await;
        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.is_repl = true;
        session.set_prompter(fixed_prompter("y"));

        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert_eq!(count(&server, "POST").await, 1);
        assert!(buf.contents().contains("enqueued"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn a_missing_server_document_has_nothing_to_lose() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(&server, rrid, ResponseTemplate::new(404)).await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert_eq!(count(&server, "POST").await, 1);
    }

    #[tokio::test]
    async fn a_stale_server_document_has_nothing_to_lose() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(&server, rrid, ResponseTemplate::new(409)).await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert_eq!(count(&server, "POST").await, 1);
    }

    #[tokio::test]
    async fn a_clean_server_document_proceeds() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(
            &server,
            rrid,
            ResponseTemplate::new(200).set_body_string(document_json(rrid, "{}")),
        )
        .await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert_eq!(count(&server, "POST").await, 1);
    }

    #[tokio::test]
    async fn an_unreadable_server_document_fails_closed() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(&server, rrid, ResponseTemplate::new(500)).await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait"]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("cannot check") && m.contains("--discard-authored")),
            "{err:?}"
        );
        assert_eq!(count(&server, "POST").await, 0);
    }

    #[tokio::test]
    async fn discard_authored_never_reads_the_server_document() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);

        let args = matches(&Regenerate, &["--no-wait", "--discard-authored"]);
        Regenerate.call(&mut session, &args).await.unwrap();

        assert_eq!(count(&server, "GET").await, 0);
        assert_eq!(count(&server, "POST").await, 1);
    }

    /// The loaded document is judged on its own: a clean server copy does not
    /// excuse content the tester has only here.
    #[tokio::test]
    async fn local_content_is_gated_even_when_the_server_document_is_clean() {
        let rrid = "SUSE:Maintenance:1:1";
        let server = MockServer::start().await;
        mount_document(
            &server,
            rrid,
            ResponseTemplate::new(200).set_body_string(document_json(rrid, "{}")),
        )
        .await;
        mount_enqueue(&server, rrid).await;
        let (mut session, _buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config = config_for(&server);
        session.metadata_mut().base_mut().document = Some(doc_with_verdict(rrid));

        let args = matches(&Regenerate, &["--no-wait"]);
        let err = Regenerate.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("loaded document")),
            "{err:?}"
        );
        assert_eq!(count(&server, "POST").await, 0);
    }
}
