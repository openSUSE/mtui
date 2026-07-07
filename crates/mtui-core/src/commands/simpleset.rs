//! Simple "set" commands (`set_log_level`).
//!
//! Ports the buildable half of upstream `mtui.commands.simpleset`. `set_workflow`
//! (which reconstructs the openQA results state) is deferred with the openQA
//! state holder (`mtui-rs-plt`); `set_timeout` already lives in
//! [`settimeout`](super::settimeout).

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::Command;
use crate::error::{CommandError, CommandResult};
use crate::session::{LogLevel, Session};

/// The log levels offered for completion / validation (upstream `choices`).
const LEVELS: &[&str] = &["info", "warning", "error", "debug"];

/// Changes the current mtui log level (upstream `SetLogLevel`).
///
/// Sets the level on the session's installed log-level sink (the REPL wires this
/// to a `tracing_subscriber::reload` handle; headless callers still log the
/// change). Setting `debug` surfaces per-command tracing in real time.
pub struct SetLogLevel;

#[async_trait]
impl Command for SetLogLevel {
    fn name(&self) -> &'static str {
        "set_log_level"
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("level")
                .required(true)
                .value_parser(clap::builder::PossibleValuesParser::new(LEVELS))
                .help("log level for mtui - info, warning, error or debug"),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        LEVELS
            .iter()
            .filter(|l| l.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let name = args
            .get_one::<String>("level")
            .ok_or_else(|| CommandError::Other("log level is required".to_owned()))?;
        let level = LogLevel::parse(name)
            .ok_or_else(|| CommandError::Other(format!("unknown log level: {name}")))?;
        session.apply_log_level(level);
        tracing::info!("Log level is set to {name}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};
    use std::sync::{Arc, Mutex};

    #[test]
    fn name_is_set_log_level() {
        assert_eq!(SetLogLevel.name(), "set_log_level");
    }

    #[test]
    fn rejects_unknown_level_at_parse_time() {
        let base = clap::Command::new("set_log_level").no_binary_name(true);
        let cmd = SetLogLevel.configure(base);
        assert!(cmd.clone().try_get_matches_from(["bogus"]).is_err());
        assert!(cmd.clone().try_get_matches_from([] as [&str; 0]).is_err());
        assert!(cmd.try_get_matches_from(["debug"]).is_ok());
    }

    #[test]
    fn completion_filters_levels_by_prefix() {
        let (session, _buf) = empty_session();
        assert_eq!(SetLogLevel.complete(&session, "de", ""), vec!["debug"]);
        let mut all = SetLogLevel.complete(&session, "", "");
        all.sort();
        assert_eq!(all, vec!["debug", "error", "info", "warning"]);
    }

    #[tokio::test]
    async fn applies_level_through_installed_sink() {
        let (mut session, _buf) = empty_session();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        session.set_log_level_sink(Box::new(move |lvl| sink_seen.lock().unwrap().push(lvl)));

        let args = matches(&SetLogLevel, &["warning"]);
        SetLogLevel.call(&mut session, &args).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![LogLevel::Warning]);
    }

    #[tokio::test]
    async fn succeeds_without_sink_installed() {
        let (mut session, _buf) = empty_session();
        let args = matches(&SetLogLevel, &["debug"]);
        // No sink installed (headless): still Ok, just logs.
        SetLogLevel.call(&mut session, &args).await.unwrap();
    }
}
