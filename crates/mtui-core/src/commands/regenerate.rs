//! The `regenerate` command — regenerate the loaded update's template.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::TeReGen;

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Regenerates the loaded update's test-report template via the TeReGen API.
///
/// Ports upstream `mtui.commands.regenerate.Regenerate`. Enqueues a regeneration
/// job (`POST /reports/{id}/regenerate`); by default waits for the Minion job to
/// finish and reports the outcome. `--force` overwrites an existing but unedited
/// template, `--ignore-inconsistent` regenerates despite inconsistent metadata,
/// and `--no-wait` enqueues and returns immediately.
///
/// Deviation from upstream: the post-success **reload** of the freshly built
/// template (upstream drops the stale checkout and calls `load_update`) is
/// deferred until `load_update` lands in mtui-core (`mtui-rs-7h2`); this command
/// reports success and tells the operator to reload.
pub struct Regenerate;

#[async_trait]
impl Command for Regenerate {
    fn name(&self) -> &'static str {
        "regenerate"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
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

        let teregen = TeReGen::new(&session.config, &session.config.teregen_api)
            .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
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

        // No interactive spinner yet (Phase 6); never stop early.
        let outcome = teregen
            .regenerate_and_wait(&rrid_str, force, ignore_inconsistent, || false)
            .await;

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

        // Success. The reload leg (drop stale checkout + load_update) is deferred
        // to mtui-rs-7h2; tell the operator to reload for now.
        session.display.println(&format!(
            "Template for {rrid_str} regenerated — reload it with load_template to pick up the new build"
        ));
        Ok(())
    }
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
