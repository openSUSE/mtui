//! The `checkers` command — list the build-check results for the loaded update.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
use crate::commands::support::{require_update, template_completion};
use crate::error::CommandResult;
use crate::session::Session;

/// Checker result strings that count as success (everything else renders red).
///
/// Ported verbatim from upstream `checkers._PASSING`.
const PASSING: &[&str] = &["passed", "success", "ok", "done"];

/// Lists the build-check (checker) result runs for the loaded update.
///
/// Ports upstream `mtui.commands.checkers.Checkers`: fetches the live checker
/// results from the TeReGen report API (`GET /reports/{id}/checkers`) and prints
/// one colored `status name` row per checker. Requires a loaded update
/// (upstream `@requires_update`).
pub struct Checkers;

#[async_trait]
impl Command for Checkers {
    fn name(&self) -> &'static str {
        "checkers"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the build-check (checker) result runs for the loaded update.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let teregen = teregen_client(session)?;

        let checkers = teregen.checkers(&rrid.to_string()).await;
        let entries = checkers.as_ref().and_then(serde_json::Value::as_array);
        let entries = match entries {
            Some(e) if !e.is_empty() => e,
            _ => {
                session
                    .display
                    .println(&format!("No checker results for {rrid}"));
                return Ok(());
            }
        };

        session
            .display
            .println(&format!("Checker results for {rrid} ({}):", entries.len()));
        for c in entries {
            let (name, status) = checker_fields(c);
            let colored = if PASSING.contains(&status.to_lowercase().as_str()) {
                session.display.green(&format!("{status:<10}"))
            } else {
                session.display.red(&format!("{status:<10}"))
            };
            session.display.println(&format!("  {colored} {name}"));
        }
        Ok(())
    }
}

/// Extracts `(name, status)` from a checker entry, mirroring upstream's
/// dict-vs-scalar handling (`name`/`status`|`state`, falling back to `?`).
fn checker_fields(c: &serde_json::Value) -> (String, String) {
    if let Some(obj) = c.as_object() {
        let name = obj
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let status = obj
            .get("status")
            .or_else(|| obj.get("state"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_owned();
        (name, status)
    } else {
        (c.as_str().unwrap_or("?").to_owned(), "?".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;
    use mtui_config::Config;

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Checkers.name(), "checkers");
        assert_eq!(Checkers.scope(), Scope::Fanout);
    }

    #[test]
    fn checker_fields_handles_dict_and_scalar() {
        let dict = serde_json::json!({"name": "rpmlint", "status": "passed"});
        assert_eq!(
            checker_fields(&dict),
            ("rpmlint".to_owned(), "passed".to_owned())
        );
        // `state` is the fallback key for status.
        let stated = serde_json::json!({"name": "x", "state": "failed"});
        assert_eq!(checker_fields(&stated).1, "failed");
        // Scalar entry.
        let scalar = serde_json::json!("bare");
        assert_eq!(checker_fields(&scalar), ("bare".to_owned(), "?".to_owned()));
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Checkers, &[]);
        let err = Checkers.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn renders_checker_rows_from_teregen() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/reports/SUSE:Maintenance:1:1/checkers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "checkers": [
                    {"name": "rpmlint", "status": "passed"},
                    {"name": "install", "status": "failed"},
                ]
            })))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("Checker results for SUSE:Maintenance:1:1 (2):"),
            "{out}"
        );
        assert!(out.contains("rpmlint"), "{out}");
        assert!(out.contains("install"), "{out}");
    }

    #[tokio::test]
    async fn reports_empty_when_no_checkers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/reports/SUSE:Maintenance:1:1/checkers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "checkers": []
            })))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents()
                .contains("No checker results for SUSE:Maintenance:1:1")
        );
    }
}
