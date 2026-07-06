//! The `list_products` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Prints the installed products on the reference hosts.
///
/// Ports upstream `mtui.commands.products.ListProducts`. Renders each selected
/// host's parsed [`System`](mtui_types::system::System) through the display's
/// `list_products` sink (base product + addons). Reflects whatever was last
/// parsed; `reload_products` refreshes it.
pub struct ListProducts;

#[async_trait]
impl Command for ListProducts {
    fn name(&self) -> &'static str {
        "list_products"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
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
        // Snapshot (hostname, system) first so the report borrow does not overlap
        // the display's mutable borrow.
        let hosts = select_names(session.targets(), args, true)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        let rows: Vec<(String, mtui_types::system::System)> = hosts
            .iter()
            .filter_map(|name| {
                session
                    .targets()
                    .get(name)
                    .map(|t| (name.clone(), t.system().clone()))
            })
            .collect();
        for (name, system) in rows {
            session.display.list_products(&name, &system);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListProducts.name(), "list_products");
        assert_eq!(ListProducts.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn lists_host_with_products_label() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListProducts, &["-t", "h1"]);
        ListProducts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // Upstream's (sic) label + the host name are rendered.
        assert!(out.contains("Referenece host"), "{out}");
        assert!(out.contains("h1"), "{out}");
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListProducts, &["-t", "ghost"]);
        let err = ListProducts.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
