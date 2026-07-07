//! The `list_metadata` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists the patchinfo metadata for the loaded test report.
///
/// Ports upstream `mtui.commands.simplelists.ListMetadata`, which calls
/// `metadata.show_yourself(sys.stdout)`. The aligned `(label, value)` rows come
/// from the report ([`show_yourself_data`](mtui_testreport::TestReport)); each
/// non-empty row is rendered as `{label:15}: {value}` (upstream `_aligned_write`).
pub struct ListMetadata;

#[async_trait]
impl Command for ListMetadata {
    fn name(&self) -> &'static str {
        "list_metadata"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rows = session.metadata().show_yourself_data();
        for (label, value) in rows {
            session.display.println(&format!("{label:15}: {value}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListMetadata.name(), "list_metadata");
        assert_eq!(ListMetadata.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn renders_only_nonempty_aligned_rows() {
        // The null report has empty Category/Reviewer/... (dropped) but the
        // report-URL rows are always non-empty, so exactly those are rendered
        // as aligned `{label:15}: {value}` lines (upstream `_aligned_write`).
        let (mut session, buf) = empty_session();
        let args = matches(&ListMetadata, &[]);
        ListMetadata.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Build checks   :"), "{out}");
        assert!(out.contains("Testreport     :"), "{out}");
        // Empty-valued rows are dropped.
        assert!(!out.contains("Category"), "{out}");
    }
}
