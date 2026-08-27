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
/// [`UpdateKind::Auto`], or [`UpdateKind::Kernel`] with `-k/--kernel`.
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
            Arg::new("rrid")
                .value_name("RRID")
                .help("template to regenerate even if not loaded (default: the loaded template)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = ["--force", "--ignore-inconsistent", "--no-wait", "-k"]
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

        // Only the fallback goes through the "load first" guard; an explicit
        // RRID breaks the load/regenerate catch-22.
        let rrid_str = match args.get_one::<String>("rrid") {
            Some(rrid) => rrid.clone(),
            None => require_update(session)?.to_string(),
        };

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
        reload(session, &rrid_str, kernel).await;
        Ok(())
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

    session.load_update(&update, false, kind).await;
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
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
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
        let args = matches(&Regenerate, &["--no-wait"]);
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

    /// A standalone `-k <RRID>` loads with the kernel workflow, proving the
    /// success path registers the RRID (`load_update`'s registration is
    /// kind-agnostic).
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

        let (mut session, _buf) = empty_session();
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
        Regenerate.call(&mut session, &args).await.unwrap();

        assert!(
            session.templates.contains(rrid),
            "RRID should be registered"
        );
        assert_eq!(session.metadata().workflow(), Workflow::Kernel);
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
}
