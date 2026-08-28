//! The `config` command (`show` / `set`).

use async_trait::async_trait;
use clap::{Arg, ArgMatches, Command as ClapCommand};
use mtui_config::{Config, SslVerify};

use crate::command::{Command, Scope};
use crate::engine::command_parser;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The configuration attributes exposed to `show`, each with a value renderer.
///
/// [`Config`] is a typed struct with no reflection, so this mapping is spelled
/// out — the one place `show`/`set` and completion agree on the surface.
fn attr_value(config: &Config, attr: &str) -> Option<String> {
    let v = match attr {
        "template_dir" => config.template_dir.display().to_string(),
        "local_tempdir" => config.local_tempdir.display().to_string(),
        "session_user" => config.session_user.clone(),
        "install_logs" => config.install_logs.display().to_string(),
        "ssl_verify" => ssl_verify_to_string(&config.ssl_verify),
        "connection_timeout" => config.connection_timeout.to_string(),
        "connect_timeout" => config.connect_timeout.to_string(),
        "reboot_timeout" => config.reboot_timeout.to_string(),
        "reboot_retries" => config.reboot_retries.to_string(),
        "max_parallel" => config.max_parallel.to_string(),
        "ssh_strict_host_key_checking" => config.ssh_strict_host_key_checking.clone(),
        "refhosts_resolvers" => config.refhosts_resolvers.clone(),
        "refhosts_https_uri" => config.refhosts_https_uri.clone(),
        "refhosts_https_expiration" => config.refhosts_https_expiration.to_string(),
        "refhosts_path" => config.refhosts_path.display().to_string(),
        "bugzilla_url" => config.bugzilla_url.clone(),
        "reports_url" => config.reports_url.clone(),
        "fancy_reports_url" => config.fancy_reports_url.clone(),
        "svn_path" => config.svn_path.clone(),
        "qem_dashboard_api" => config.qem_dashboard_api.clone(),
        "teregen_api" => config.teregen_api.clone(),
        "openqa_instance" => config.openqa_instance.clone(),
        "openqa_instance_baremetal" => config.openqa_instance_baremetal.clone(),
        "openqa_install_distri" => config.openqa_install_distri.clone(),
        // Never verbatim: it would land in scrollback and logs.
        "gitea_token" => {
            if config.gitea_token.is_empty() {
                String::new()
            } else {
                SECRET_MASK.to_owned()
            }
        }
        "gitea_url" => config.gitea_url.clone(),
        // Also a secret.
        "slack_token" => {
            if config.slack_token.is_empty() {
                String::new()
            } else {
                SECRET_MASK.to_owned()
            }
        }
        "slack_enabled" => config.slack_enabled.to_string(),
        "slack_channel" => config.slack_channel.clone(),
        "slack_api_url" => config.slack_api_url.clone(),
        "slack_poll_interval" => config.slack_poll_interval.to_string(),
        "slack_watch_timeout" => config.slack_watch_timeout.to_string(),
        "target_tempdir" => config.target_tempdir.display().to_string(),
        "lock_reap_stale" => config.lock_reap_stale.to_string(),
        "lock_stale_age" => config.lock_stale_age.to_string(),
        "pool_reap_stale" => config.pool_reap_stale.to_string(),
        "pool_stale_age" => config.pool_stale_age.to_string(),
        "lock_pi_autolock" => config.lock_pi_autolock.to_string(),
        "lock_wait" => config.lock_wait.to_string(),
        "lock_wait_poll" => config.lock_wait_poll.to_string(),
        _ => return None,
    };
    Some(v)
}

/// Stands in for a set secret, so a credential never reaches the display buffer
/// (terminal scrollback or MCP output) via `show` or `set`.
const SECRET_MASK: &str = "<set>";

/// Whether `attr` names a secret whose value must never be echoed. The single
/// source of truth for `show`'s mask and `set`'s redacted acknowledgement.
fn is_secret_attr(attr: &str) -> bool {
    matches!(attr, "gitea_token" | "slack_token")
}

/// Renders an [`SslVerify`] back to the form `config set` and the config file
/// accept, so `show` round-trips into `set`.
fn ssl_verify_to_string(v: &SslVerify) -> String {
    match v {
        SslVerify::Enabled => "true".to_owned(),
        SslVerify::Disabled => "false".to_owned(),
        SslVerify::CaBundle(path) => path.display().to_string(),
    }
}

/// The attribute names `show` lists when given none, in a stable order.
const ATTRS: [&str; 40] = [
    "template_dir",
    "local_tempdir",
    "session_user",
    "install_logs",
    "ssl_verify",
    "connection_timeout",
    "connect_timeout",
    "reboot_timeout",
    "reboot_retries",
    "max_parallel",
    "ssh_strict_host_key_checking",
    "refhosts_resolvers",
    "refhosts_https_uri",
    "refhosts_https_expiration",
    "refhosts_path",
    "bugzilla_url",
    "reports_url",
    "fancy_reports_url",
    "svn_path",
    "qem_dashboard_api",
    "teregen_api",
    "openqa_instance",
    "openqa_instance_baremetal",
    "openqa_install_distri",
    "gitea_token",
    "gitea_url",
    "slack_enabled",
    "slack_token",
    "slack_channel",
    "slack_api_url",
    "slack_poll_interval",
    "slack_watch_timeout",
    "target_tempdir",
    "lock_reap_stale",
    "lock_stale_age",
    "pool_reap_stale",
    "pool_stale_age",
    "lock_pi_autolock",
    "lock_wait",
    "lock_wait_poll",
];

/// Parses `raw` for `attr` and stores it. An invalid value leaves the
/// attribute unchanged.
fn set_attr(config: &mut Config, attr: &str, raw: &str) -> Result<(), String> {
    let parse_bool = |s: &str| match s {
        "1" | "yes" | "true" | "on" => Ok(true),
        "0" | "no" | "false" | "off" => Ok(false),
        other => Err(format!("invalid boolean: {other}")),
    };
    let parse_u64 = |s: &str| {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer: {e}"))
    };
    // For the keys the loader guards with `validated_positive!`: runtime `set`
    // must reject 0 too, or it could store a value the file would refuse.
    let parse_positive_u64 = |s: &str| match s.parse::<u64>() {
        Ok(0) => Err("expected a positive integer greater than 0".to_owned()),
        Ok(value) => Ok(value),
        Err(e) => Err(format!("invalid integer: {e}")),
    };

    match attr {
        "session_user" => config.session_user = raw.to_owned(),
        "ssh_strict_host_key_checking" => config.ssh_strict_host_key_checking = raw.to_owned(),
        "refhosts_resolvers" => config.refhosts_resolvers = raw.to_owned(),
        "refhosts_https_uri" => config.refhosts_https_uri = raw.to_owned(),
        "bugzilla_url" => config.bugzilla_url = raw.to_owned(),
        "reports_url" => config.reports_url = raw.to_owned(),
        "fancy_reports_url" => config.fancy_reports_url = raw.to_owned(),
        "svn_path" => config.svn_path = raw.to_owned(),
        "qem_dashboard_api" => config.qem_dashboard_api = raw.to_owned(),
        "teregen_api" => config.teregen_api = raw.to_owned(),
        "openqa_instance" => config.openqa_instance = raw.to_owned(),
        "openqa_instance_baremetal" => config.openqa_instance_baremetal = raw.to_owned(),
        "openqa_install_distri" => config.openqa_install_distri = raw.to_owned(),
        "gitea_token" => config.gitea_token = raw.to_owned(),
        "gitea_url" => config.gitea_url = raw.to_owned(),
        "slack_token" => config.slack_token = raw.to_owned(),
        "slack_channel" => config.slack_channel = raw.to_owned(),
        "slack_api_url" => config.slack_api_url = raw.to_owned(),
        "slack_enabled" => config.slack_enabled = parse_bool(raw)?,
        "slack_poll_interval" => config.slack_poll_interval = parse_positive_u64(raw)?,
        "slack_watch_timeout" => config.slack_watch_timeout = parse_positive_u64(raw)?,
        // The same coercion as config-file loading, so a runtime `set` cannot
        // store a value the file would reject.
        "ssl_verify" => config.ssl_verify = SslVerify::parse(raw),
        "lock_reap_stale" => config.lock_reap_stale = parse_bool(raw)?,
        "pool_reap_stale" => config.pool_reap_stale = parse_bool(raw)?,
        "lock_pi_autolock" => config.lock_pi_autolock = parse_bool(raw)?,
        "connection_timeout" => config.connection_timeout = parse_positive_u64(raw)?,
        "connect_timeout" => config.connect_timeout = parse_positive_u64(raw)?,
        "reboot_timeout" => config.reboot_timeout = parse_positive_u64(raw)?,
        "reboot_retries" => config.reboot_retries = parse_positive_u64(raw)?,
        "max_parallel" => config.max_parallel = parse_positive_u64(raw)?,
        "refhosts_https_expiration" => config.refhosts_https_expiration = parse_positive_u64(raw)?,
        "lock_stale_age" => config.lock_stale_age = parse_u64(raw)?,
        "pool_stale_age" => config.pool_stale_age = parse_u64(raw)?,
        "lock_wait" => config.lock_wait = parse_u64(raw)?,
        "lock_wait_poll" => config.lock_wait_poll = parse_positive_u64(raw)?,
        other => return Err(format!("unknown or read-only attribute: {other}")),
    }
    Ok(())
}

/// Shows or sets runtime configuration values.
///
/// `config show [attr ...]` prints the current values (all when none named).
/// `config set <attr> <value>` validates at least as strictly as config-file
/// loading, so it cannot store a value the file would reject. Self-describing,
/// so it runs once ([`Scope::Single`]).
pub struct ConfigCmd;

#[async_trait]
impl Command for ConfigCmd {
    fn name(&self) -> &'static str {
        "config"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Shows or sets runtime configuration values.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    /// `set` writes `session.config`, which a fork clones by value (#523).
    ///
    /// Only a positively-identified `show` is scoped: an unknown subcommand, an
    /// empty argv and a parse failure fall through to the canonical session —
    /// safe by default, but that gate is exclusive and writer-preference, so add
    /// a read-only subcommand here or it silently queues behind background jobs.
    fn requires_canonical_session(&self, argv: &[String]) -> bool {
        !matches!(
            command_parser(self).try_get_matches_from(argv),
            Ok(m) if m.subcommand_name() == Some("show")
        )
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.subcommand_required(true)
            .subcommand(
                ClapCommand::new("show").arg(
                    Arg::new("attributes")
                        .num_args(0..)
                        .value_name("ATTR")
                        .help("Attribute(s) to show; all when omitted"),
                ),
            )
            .subcommand(
                ClapCommand::new("set")
                    .arg(Arg::new("attribute").required(true).value_name("ATTR"))
                    .arg(Arg::new("value").required(true).value_name("VALUE")),
            )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        match args.subcommand() {
            Some(("show", sub)) => {
                let requested: Vec<String> = sub
                    .get_many::<String>("attributes")
                    .map(|it| it.cloned().collect())
                    .unwrap_or_default();
                let attrs: Vec<String> = if requested.is_empty() {
                    ATTRS.iter().map(|s| (*s).to_owned()).collect()
                } else {
                    requested
                };
                let width = attrs.iter().map(String::len).max().unwrap_or(0);
                let mut rows: Vec<String> = Vec::new();
                for attr in &attrs {
                    match attr_value(&session.config, attr) {
                        Some(v) => rows.push(format!("{attr:<width$} = {v:?}")),
                        None => {
                            return Err(CommandError::Other(format!("unknown attribute: {attr}")));
                        }
                    }
                }
                for row in rows {
                    session.display.println(&row);
                }
                Ok(())
            }
            Some(("set", sub)) => {
                let attr = sub.get_one::<String>("attribute").expect("required");
                let value = sub.get_one::<String>("value").expect("required");
                set_attr(&mut session.config, attr, value).map_err(CommandError::Other)?;
                // Never echo a secret back to the display buffer.
                let shown = if is_secret_attr(attr) {
                    SECRET_MASK
                } else {
                    value
                };
                session
                    .display
                    .println(&format!("option: {attr} set to value : {shown}"));
                Ok(())
            }
            _ => Err(CommandError::Other(
                "config: expected `show` or `set`".to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(ConfigCmd.name(), "config");
        assert_eq!(ConfigCmd.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn show_one_attribute() {
        let (mut session, buf) = empty_session();
        session.config.session_user = "alice".to_owned();
        let args = matches(&ConfigCmd, &["show", "session_user"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("session_user"));
        assert!(buf.contents().contains("\"alice\""));
    }

    #[tokio::test]
    async fn show_all_lists_every_attr() {
        let (mut session, buf) = empty_session();
        let args = matches(&ConfigCmd, &["show"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("template_dir"));
        assert!(out.contains("lock_wait_poll"));
        // These load and are honored, so they must appear in the table too.
        assert!(out.contains("ssl_verify"));
        assert!(out.contains("qem_dashboard_api"));
        assert!(out.contains("teregen_api"));
        assert!(out.contains("openqa_instance"));
        assert!(out.contains("openqa_instance_baremetal"));
        assert!(out.contains("openqa_install_distri"));
        assert!(out.contains("gitea_token"));
    }

    #[tokio::test]
    async fn show_ssl_verify_renders_enum_forms() {
        let (mut session, buf) = empty_session();
        session.config.ssl_verify = SslVerify::Enabled;
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "ssl_verify"]))
            .await
            .unwrap();
        assert!(buf.contents().contains("ssl_verify"));
        assert!(buf.contents().contains("\"true\""));

        let (mut session, buf) = empty_session();
        session.config.ssl_verify = SslVerify::Disabled;
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "ssl_verify"]))
            .await
            .unwrap();
        assert!(buf.contents().contains("\"false\""));

        let (mut session, buf) = empty_session();
        session.config.ssl_verify = SslVerify::CaBundle(std::path::PathBuf::from("/etc/ca.pem"));
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "ssl_verify"]))
            .await
            .unwrap();
        assert!(buf.contents().contains("/etc/ca.pem"));
    }

    #[tokio::test]
    async fn set_ssl_verify_bool_and_path() {
        let (mut session, _buf) = empty_session();
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "ssl_verify", "false"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.ssl_verify, SslVerify::Disabled);

        // A non-boolean value becomes a CA-bundle path.
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "ssl_verify", "/x/ca.pem"]),
            )
            .await
            .unwrap();
        assert_eq!(
            session.config.ssl_verify,
            SslVerify::CaBundle(std::path::PathBuf::from("/x/ca.pem"))
        );
    }

    #[tokio::test]
    async fn set_datasource_url_and_token() {
        let (mut session, _buf) = empty_session();
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "openqa_instance", "http://oqa.local"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.openqa_instance, "http://oqa.local");

        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "gitea_token", "secret123"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.gitea_token, "secret123");
    }

    #[tokio::test]
    async fn show_gitea_token_is_masked() {
        let (mut session, buf) = empty_session();
        session.config.gitea_token = "secret123".to_owned();
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "gitea_token"]))
            .await
            .unwrap();
        let out = buf.contents();
        assert!(!out.contains("secret123"));
        assert!(out.contains("<set>"));
    }

    #[tokio::test]
    async fn set_secret_attr_does_not_echo_value() {
        let (mut session, buf) = empty_session();
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "gitea_token", "secret123"]),
            )
            .await
            .unwrap();
        // Stored, but never echoed.
        assert_eq!(session.config.gitea_token, "secret123");
        let out = buf.contents();
        assert!(out.contains("gitea_token"));
        assert!(!out.contains("secret123"));
        assert!(out.contains("<set>"));
    }

    #[tokio::test]
    async fn show_unknown_attr_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["show", "nope"]);
        let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("unknown attribute")));
    }

    #[tokio::test]
    async fn set_string_updates_value() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "session_user", "bob"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert_eq!(session.config.session_user, "bob");
    }

    #[tokio::test]
    async fn set_and_show_max_parallel_roundtrips() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "max_parallel", "8"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert_eq!(session.config.max_parallel, 8);
        assert_eq!(
            attr_value(&session.config, "max_parallel").as_deref(),
            Some("8")
        );
    }

    #[tokio::test]
    async fn set_and_show_reboot_backoff_roundtrips() {
        let (mut session, _buf) = empty_session();
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "reboot_timeout", "20"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.reboot_timeout, 20);
        assert_eq!(
            attr_value(&session.config, "reboot_timeout").as_deref(),
            Some("20")
        );

        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "reboot_retries", "5"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.reboot_retries, 5);
        assert_eq!(
            attr_value(&session.config, "reboot_retries").as_deref(),
            Some("5")
        );
    }

    #[tokio::test]
    async fn set_bool_parses_config_spellings() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "lock_reap_stale", "no"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert!(!session.config.lock_reap_stale);
    }

    #[tokio::test]
    async fn set_invalid_bool_rejected_and_unchanged() {
        let (mut session, _buf) = empty_session();
        let before = session.config.lock_reap_stale;
        let args = matches(&ConfigCmd, &["set", "lock_reap_stale", "maybe"]);
        let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("invalid boolean")));
        assert_eq!(session.config.lock_reap_stale, before);
    }

    #[tokio::test]
    async fn set_unknown_attr_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "nope", "x"]);
        let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("unknown or read-only")));
    }

    #[tokio::test]
    async fn set_zero_rejected_for_positive_only_keys() {
        // `validated_positive!` in the loader guards these, so runtime `set`
        // must reject 0 too.
        for attr in [
            "connection_timeout",
            "connect_timeout",
            "reboot_timeout",
            "reboot_retries",
            "max_parallel",
            "refhosts_https_expiration",
            "slack_poll_interval",
            "slack_watch_timeout",
            "lock_wait_poll",
        ] {
            let (mut session, _buf) = empty_session();
            let before = attr_value(&session.config, attr);
            let args = matches(&ConfigCmd, &["set", attr, "0"]);
            let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("positive integer greater than 0")),
                "{attr}: expected a positive-integer rejection, got {err:?}"
            );
            assert_eq!(
                attr_value(&session.config, attr),
                before,
                "{attr}: value must be unchanged after a rejected set"
            );
        }
    }

    #[tokio::test]
    async fn set_zero_allowed_for_keys_the_loader_also_accepts() {
        // The loader takes these via `unwrap_or`, so 0 is valid and the
        // positive-only guard must not over-reach. A non-zero value is set first
        // so `lock_wait`'s already-0 default cannot mask a no-op.
        for attr in ["lock_stale_age", "lock_wait"] {
            let (mut session, _buf) = empty_session();
            ConfigCmd
                .call(&mut session, &matches(&ConfigCmd, &["set", attr, "9"]))
                .await
                .unwrap();
            ConfigCmd
                .call(&mut session, &matches(&ConfigCmd, &["set", attr, "0"]))
                .await
                .unwrap();
            assert_eq!(attr_value(&session.config, attr).as_deref(), Some("0"));
        }
    }

    #[tokio::test]
    async fn set_positive_expiration_roundtrips() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "refhosts_https_expiration", "3600"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert_eq!(
            attr_value(&session.config, "refhosts_https_expiration").as_deref(),
            Some("3600")
        );
    }

    #[tokio::test]
    async fn set_pool_reap_keys_roundtrip() {
        let (mut session, _buf) = empty_session();
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "pool_reap_stale", "false"]),
            )
            .await
            .unwrap();
        assert_eq!(
            attr_value(&session.config, "pool_reap_stale").as_deref(),
            Some("false")
        );
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "pool_stale_age", "3600"]),
            )
            .await
            .unwrap();
        assert_eq!(
            attr_value(&session.config, "pool_stale_age").as_deref(),
            Some("3600")
        );
        // 0 disables reaping and must be accepted.
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "pool_stale_age", "0"]),
            )
            .await
            .unwrap();
        assert_eq!(
            attr_value(&session.config, "pool_stale_age").as_deref(),
            Some("0")
        );
    }

    #[tokio::test]
    async fn set_non_numeric_on_positive_key_is_invalid_integer_not_range_error() {
        // The two failure modes stay distinct.
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "max_parallel", "abc"]);
        let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m)
                if m.contains("invalid integer") && !m.contains("positive integer")),
            "expected an invalid-integer error, got {err:?}"
        );
    }
}
