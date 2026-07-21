//! The `config` command (`show` / `set`).

use async_trait::async_trait;
use clap::{Arg, ArgMatches, Command as ClapCommand};
use mtui_config::{Config, SslVerify};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The configuration attributes exposed to `show`, each with a value renderer.
///
/// Ports the read side of upstream `config show`, which iterates
/// `config.data`. The Rust [`Config`] is a typed struct (no reflection), so the
/// attribute↔value mapping is spelled out here — the single place `show`/`set`
/// and completion agree on the attribute surface.
fn attr_value(config: &Config, attr: &str) -> Option<String> {
    let v = match attr {
        "template_dir" => config.template_dir.display().to_string(),
        "local_tempdir" => config.local_tempdir.display().to_string(),
        "session_user" => config.session_user.clone(),
        "install_logs" => config.install_logs.display().to_string(),
        "chdir_to_template_dir" => config.chdir_to_template_dir.to_string(),
        "ssl_verify" => ssl_verify_to_string(&config.ssl_verify),
        "connection_timeout" => config.connection_timeout.to_string(),
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
        // A secret: never render the token verbatim (it lands in scrollback and
        // logs). Show only whether one is set, mirroring how `show` should treat
        // credentials while still confirming presence.
        "gitea_token" => {
            if config.gitea_token.is_empty() {
                String::new()
            } else {
                SECRET_MASK.to_owned()
            }
        }
        "gitea_url" => config.gitea_url.clone(),
        // Also a secret, masked on the same terms as the Gitea token.
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
        "lock_pi_autolock" => config.lock_pi_autolock.to_string(),
        "lock_wait" => config.lock_wait.to_string(),
        "lock_wait_poll" => config.lock_wait_poll.to_string(),
        _ => return None,
    };
    Some(v)
}

/// The placeholder shown in place of a set secret value, so a credential never
/// reaches the display buffer (terminal scrollback or MCP output) via `show` or
/// `set`.
const SECRET_MASK: &str = "<set>";

/// Whether `attr` names a secret configuration field whose value must never be
/// echoed. The single source of truth for `show`'s mask and `set`'s redacted
/// acknowledgement; add a new secret field here to cover both paths at once.
fn is_secret_attr(attr: &str) -> bool {
    matches!(attr, "gitea_token" | "slack_token")
}

/// Render an [`SslVerify`] back to the string form `config set`/the config file
/// accept, so `show` round-trips into `set`: `"true"`/`"false"` for the
/// enabled/disabled postures, or the CA-bundle path verbatim.
fn ssl_verify_to_string(v: &SslVerify) -> String {
    match v {
        SslVerify::Enabled => "true".to_owned(),
        SslVerify::Disabled => "false".to_owned(),
        SslVerify::CaBundle(path) => path.display().to_string(),
    }
}

/// The attribute names `show` lists when given none, in a stable order.
const ATTRS: [&str; 36] = [
    "template_dir",
    "local_tempdir",
    "session_user",
    "install_logs",
    "chdir_to_template_dir",
    "ssl_verify",
    "connection_timeout",
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
    "lock_pi_autolock",
    "lock_wait",
    "lock_wait_poll",
];

/// Parses `raw` for `attr` and stores it, mirroring upstream's getter/fixup
/// rejection (an invalid value leaves the attribute unchanged).
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
    // Positive-only variant for the keys the config-file loader guards with
    // `validated_positive!` (0 is rejected there, falling back to the default).
    // Runtime `set` must reject 0 too, so it cannot store a value the file would
    // refuse — the guarantee this command's doc comment makes. Keys the loader
    // accepts 0 for (`lock_stale_age`, `lock_wait`, via `unwrap_or`) keep the
    // plain `parse_u64`.
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
        // Goes through the same coercion as config-file loading (a boolean
        // spelling toggles verification, anything else is a CA-bundle path), so
        // a runtime `set` cannot store a value the file would reject.
        "ssl_verify" => config.ssl_verify = SslVerify::parse(raw),
        "chdir_to_template_dir" => config.chdir_to_template_dir = parse_bool(raw)?,
        "lock_reap_stale" => config.lock_reap_stale = parse_bool(raw)?,
        "lock_pi_autolock" => config.lock_pi_autolock = parse_bool(raw)?,
        "connection_timeout" => config.connection_timeout = parse_positive_u64(raw)?,
        "max_parallel" => config.max_parallel = parse_positive_u64(raw)?,
        "refhosts_https_expiration" => config.refhosts_https_expiration = parse_positive_u64(raw)?,
        "lock_stale_age" => config.lock_stale_age = parse_u64(raw)?,
        "lock_wait" => config.lock_wait = parse_u64(raw)?,
        "lock_wait_poll" => config.lock_wait_poll = parse_positive_u64(raw)?,
        other => return Err(format!("unknown or read-only attribute: {other}")),
    }
    Ok(())
}

/// Shows or sets runtime configuration values.
///
/// Ports upstream `mtui.commands.config.Config` (the command). `config show
/// [attr ...]` prints the current values (all when none named); `config set
/// <attr> <value>` updates one, going through value parsing/validation at least
/// as strict as config-file loading, so a runtime `set` cannot store a value the
/// file would reject. Self-describing, so it runs once ([`Scope::Single`]).
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
                // Never echo a secret's value back to the display buffer (which
                // reaches terminal scrollback and MCP output); confirm the set
                // with the mask instead.
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
        // Regression: ssl_verify and the later-phase datasource options must be
        // visible in `show` (they load and are honored, but were missing from
        // the command's attribute table).
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
        // Default posture renders as "true".
        session.config.ssl_verify = SslVerify::Enabled;
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "ssl_verify"]))
            .await
            .unwrap();
        assert!(buf.contents().contains("ssl_verify"));
        assert!(buf.contents().contains("\"true\""));

        // Disabled renders as "false".
        let (mut session, buf) = empty_session();
        session.config.ssl_verify = SslVerify::Disabled;
        ConfigCmd
            .call(&mut session, &matches(&ConfigCmd, &["show", "ssl_verify"]))
            .await
            .unwrap();
        assert!(buf.contents().contains("\"false\""));

        // A CA bundle path is shown verbatim.
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
        // Boolean spelling disables verification.
        ConfigCmd
            .call(
                &mut session,
                &matches(&ConfigCmd, &["set", "ssl_verify", "false"]),
            )
            .await
            .unwrap();
        assert_eq!(session.config.ssl_verify, SslVerify::Disabled);

        // A non-boolean value becomes a CA-bundle path (config-file coercion).
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
        // The secret is never rendered verbatim; presence is confirmed instead.
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
        // The value is still stored, but never echoed to the display buffer.
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
    async fn set_bool_parses_config_spellings() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ConfigCmd, &["set", "chdir_to_template_dir", "yes"]);
        ConfigCmd.call(&mut session, &args).await.unwrap();
        assert!(session.config.chdir_to_template_dir);
    }

    #[tokio::test]
    async fn set_invalid_bool_rejected_and_unchanged() {
        let (mut session, _buf) = empty_session();
        let before = session.config.chdir_to_template_dir;
        let args = matches(&ConfigCmd, &["set", "chdir_to_template_dir", "maybe"]);
        let err = ConfigCmd.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("invalid boolean")));
        assert_eq!(session.config.chdir_to_template_dir, before);
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
        // These keys are guarded by `validated_positive!` in the config-file
        // loader, so runtime `set` must reject 0 too — otherwise it could store a
        // value the file would refuse, breaking the command's own contract.
        for attr in [
            "connection_timeout",
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
        // `lock_stale_age` / `lock_wait` are taken via `unwrap_or` in the loader
        // (0 is a valid value there), so the positive-only guard must not
        // over-reach to keys the file format permits. Set a non-zero value first,
        // then 0, so the final assertion proves 0 was actually stored (lock_wait's
        // default is already 0, which would otherwise mask a no-op).
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
    async fn set_non_numeric_on_positive_key_is_invalid_integer_not_range_error() {
        // A non-numeric value must surface the parse error, not the positive-only
        // rejection — the two failure modes stay distinct.
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
