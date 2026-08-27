//! The `list_metadata` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists the patchinfo metadata for the loaded test report: each non-empty
/// `(label, value)` row from
/// [`show_yourself_data`](mtui_testreport::TestReport), as `{label:15}: {value}`.
pub struct ListMetadata;

#[async_trait]
impl Command for ListMetadata {
    fn name(&self) -> &'static str {
        "list_metadata"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the patchinfo metadata for the loaded test report.")
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
        // The null report's Category/Reviewer/... are empty and dropped; its
        // report-URL rows never are.
        let (mut session, buf) = empty_session();
        let args = matches(&ListMetadata, &[]);
        ListMetadata.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Build checks   :"), "{out}");
        assert!(out.contains("Testreport     :"), "{out}");
        assert!(!out.contains("Category"), "{out}");
    }
}
