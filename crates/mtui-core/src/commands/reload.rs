//! The `reload_products` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, host_op_budget, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Reloads and re-parses the products on the target reference hosts.
///
/// Re-runs the system/product parse over each selected host's live connection
/// via [`Target::reload_system`](mtui_hosts::Target::reload_system).
/// Best-effort: a host whose parse fails logs a warning and keeps its
/// previously recorded system. Each host's parse runs under its own
/// [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT) budget; a host
/// that exhausts it keeps its previous system, the pass continues, and the
/// command fails naming the hosts it could not reach.
///
/// A host-phase command that takes only `-t/--target`, so it is
/// [`Scope::Active`]: it acts on the active template's host set, not once per
/// loaded template.
pub struct ReloadProducts;

#[async_trait]
impl Command for ReloadProducts {
    fn name(&self) -> &'static str {
        "reload_products"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Reloads and re-parses the products on the target reference hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Active
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .targets()
            .names()
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        // `reload_system` re-parses the host's product set in place and does not
        // return an outcome (a parse failure only logs and keeps the previous
        // system — P3a-1 did not add a reload accessor, out of scope here). Read
        // the (possibly refreshed) base product back and confirm per host so the
        // command is never silent for an MCP caller.
        let mut lines: Vec<String> = Vec::new();
        let mut unreached: Vec<&str> = Vec::new();
        let budget = host_op_budget();
        let secs = budget.as_secs();
        // Per host, not one budget for the whole (serial) pass — see
        // `host_op_budget`. A host that exhausts its own is abandoned and the
        // pass moves on.
        for name in &hosts {
            if let Some(t) = targets.get_mut(name) {
                if tokio::time::timeout(budget, t.reload_system())
                    .await
                    .is_err()
                {
                    unreached.push(name);
                    lines.push(format!("{name}: reload not completed within {secs}s"));
                    continue;
                }
                tracing::info!(host = %name, "reloaded products");
                let product = t.system().get_base().name.clone();
                lines.push(format!("{name}: reloaded products ({product})"));
            }
        }

        for line in &lines {
            session.display.println(line);
        }
        if unreached.is_empty() {
            return Ok(());
        }
        Err(CommandError::Other(format!(
            "reload_products timed out on: {}",
            unreached.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};
    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::TargetState;

    #[test]
    fn name_and_active_scope() {
        assert_eq!(ReloadProducts.name(), "reload_products");
        // Upstream `ReloadProducts` is `-t`-only (no `_add_template_arg`), so it
        // stays active rather than fanning out per loaded template.
        assert_eq!(ReloadProducts.scope(), Scope::Active);
    }

    #[tokio::test]
    async fn reloads_system_over_live_connection() {
        let _budget = crate::commands::testkit::hold_host_op_budget().await;
        // A SUSE host whose product XML parses to SLES 15-SP5.
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec());
        let target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "ok");
        session.targets_mut().add(target);
        assert_eq!(
            session
                .targets()
                .get("h1")
                .unwrap()
                .system()
                .get_base()
                .name,
            "unknown"
        );

        let args = matches(&ReloadProducts, &["-t", "h1"]);
        ReloadProducts.call(&mut session, &args).await.unwrap();

        assert_eq!(
            session
                .targets()
                .get("h1")
                .unwrap()
                .system()
                .get_base()
                .name,
            "SLES"
        );
        // The confirmation names the host and the freshly re-parsed product.
        assert!(
            buf.contents().contains("h1: reloaded products (SLES)"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ReloadProducts, &["-t", "ghost"]);
        let err = ReloadProducts.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn reload_is_bounded_and_the_pass_continues_past_a_wedged_host() {
        use std::time::Duration;

        use crate::commands::testkit::{self, session_with_targets};

        // Hosts are visited in sorted order, so "a-dead" wedges before "z-good"
        // is reached: with a per-host budget the pass must survive it and still
        // reload the later host. `a-dead` answers no SFTP at all —
        // `parse_system`'s first act is opening an SFTP session, so no product
        // scripting is needed (or wanted: an unscripted listing would fail fast
        // instead).
        let _budget = testkit::shrink_host_op_budget(50).await;

        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let good = MockConnection::new("z-good")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec());
        let dead = MockConnection::new("a-dead").with_sftp_session_delay(Duration::from_secs(3600));
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![
                Target::with_connection("a-dead", TargetState::Enabled, Box::new(dead)),
                Target::with_connection("z-good", TargetState::Enabled, Box::new(good)),
            ],
        );

        let args = matches(&ReloadProducts, &[]);
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            ReloadProducts.call(&mut session, &args),
        )
        .await
        .expect("reload_products must return despite the wedged host")
        .expect_err("a host that never answered must not report success");

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("a-dead")),
            "{err}"
        );
        let out = buf.contents();
        // The host after the wedge was still reached; the wedged one is named as
        // unfinished, never as reloaded.
        assert!(out.contains("z-good: reloaded products (SLES)"), "{out}");
        assert!(!out.contains("a-dead: reloaded products"), "{out}");
        assert!(out.contains("a-dead: reload not completed"), "{out}");
    }
}
