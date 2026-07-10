//! The `load_template` command.

use async_trait::async_trait;
use clap::{Arg, ArgGroup, ArgMatches};
use mtui_testreport::UpdateKind;
use mtui_types::UpdateID;

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Loads a maintenance-update template into the session and connects its
/// reference hosts.
///
/// Ports upstream `mtui.commands.loadtemplate.LoadTemplate`. Exactly one of
/// `-a`/`--auto-review-id` (an automatic OBS update) or `-k`/`--kernel-review-id`
/// (a kernel/live-patch update) names the update by RRID; the two are mutually
/// exclusive and one is required. The template is **added** to the registry
/// (keyed by RRID) and made active — re-loading an already-loaded RRID replaces
/// its stored report and re-activates it, leaving sibling templates untouched.
///
/// It names its own target RRID via `-a`/`-k` and connects only that template's
/// reference hosts, so it runs once ([`Scope::Single`]) regardless of how many
/// templates are loaded — without this it would fan out under MCP and re-run the
/// autoconnect (grabbing pool hosts) on every loaded template.
///
/// The `-a` update autoconnects its reference hosts by default (upstream
/// `AutoOBSUpdateID.make_testreport(autoconnect=True)`); `-k` starts the kernel
/// workflow and does not autoconnect on load (upstream `KernelOBSUpdateID`
/// defaults `autoconnect=False`). The autoconnect intent is honoured by
/// [`Session::load_update`], which the concrete workflow selection flows through.
pub struct LoadTemplate;

#[async_trait]
impl Command for LoadTemplate {
    fn name(&self) -> &'static str {
        "load_template"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Loads a maintenance-update template into the session and connects its reference hosts.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("auto")
                .short('a')
                .long("auto-review-id")
                .value_name("RequestReviewID")
                .help("OBS request review id, e.g. SUSE:Maintenance:1:1"),
        )
        .arg(
            Arg::new("kernel")
                .short('k')
                .long("kernel-review-id")
                .value_name("RequestReviewID")
                .help("OBS kernel/live-patch request review id, e.g. SUSE:Maintenance:1:1"),
        )
        // Mutually exclusive, and exactly one is required (upstream
        // `add_mutually_exclusive_group(required=True)`).
        .group(
            ArgGroup::new("review_id")
                .args(["auto", "kernel"])
                .required(true)
                .multiple(false),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        // Upstream offers the two RRID prefixes plus the flags.
        [
            "SUSE:Maintenance:",
            "openSUSE:Maintenance:",
            "-a",
            "--auto-review-id",
            "-k",
            "--kernel-review-id",
        ]
        .into_iter()
        .filter(|c| c.starts_with(text))
        .map(str::to_owned)
        .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // Exactly one of the two flags is present (enforced by the required
        // mutually-exclusive group), naming the update kind and its RRID.
        let (rrid, kind) = match (
            args.get_one::<String>("auto"),
            args.get_one::<String>("kernel"),
        ) {
            (Some(rrid), None) => (rrid, UpdateKind::Auto),
            (None, Some(rrid)) => (rrid, UpdateKind::Kernel),
            // Unreachable: the ArgGroup guarantees exactly one is set.
            _ => {
                return Err(CommandError::Other(
                    "load_template requires exactly one of -a/-k".to_owned(),
                ));
            }
        };

        let update = UpdateID::parse(rrid)
            .map_err(|e| CommandError::Other(format!("invalid RRID {rrid:?}: {e}")))?;

        // `load_update` builds the report (workflow seeded from `kind`), adds it
        // to the registry, activates it, and — for an autoconnecting update —
        // connects its reference hosts. autoconnect is always requested here
        // (upstream `load_update(..., autoconnect=True)`); the update kind
        // decides whether a connect actually happens.
        let loaded = session.load_update(&update, true, kind).await;
        if loaded.is_empty() {
            return Err(CommandError::Other(format!(
                "could not load template for {rrid}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(LoadTemplate.name(), "load_template");
        assert_eq!(LoadTemplate.scope(), Scope::Single);
    }

    #[test]
    fn requires_exactly_one_review_id() {
        // Neither flag → clap rejects (required group).
        let cmd = LoadTemplate.configure(clap::Command::new("load_template"));
        assert!(cmd.clone().try_get_matches_from(["load_template"]).is_err());
        // Both flags → mutually exclusive, rejected.
        assert!(
            cmd.clone()
                .try_get_matches_from([
                    "load_template",
                    "-a",
                    "SUSE:Maintenance:1:1",
                    "-k",
                    "SUSE:Maintenance:2:2",
                ])
                .is_err()
        );
        // Exactly one → accepted.
        assert!(
            cmd.try_get_matches_from(["load_template", "-a", "SUSE:Maintenance:1:1"])
                .is_ok()
        );
    }

    #[tokio::test]
    async fn invalid_rrid_is_reported() {
        let (mut session, _buf) = empty_session();
        let args = matches(&LoadTemplate, &["-a", "not-an-rrid"]);
        let err = LoadTemplate.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("invalid RRID")));
    }

    #[tokio::test]
    async fn unloadable_template_reports_error() {
        // An RRID that cannot be checked out (empty template_dir, offline svn)
        // surfaces as a clear "could not load" error rather than a phantom
        // registration.
        let (mut session, _buf) = empty_session();
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&LoadTemplate, &["-k", "SUSE:Maintenance:1:1"]);
        let err = LoadTemplate.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("could not load")));
        assert!(session.templates.is_empty());
    }

    #[tokio::test]
    async fn kernel_load_registers_and_activates() {
        // A kernel load of an on-disk template registers + activates it without
        // connecting (kernel does not autoconnect).
        let (mut session, _buf) = empty_session();
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();
        session.config.template_dir = tmp.path().to_path_buf();

        let args = matches(&LoadTemplate, &["-k", rrid]);
        LoadTemplate.call(&mut session, &args).await.unwrap();

        assert!(session.templates.contains(rrid));
        assert_eq!(session.templates.active_rrid(), Some(rrid));
        assert!(session.targets().is_empty());
    }

    #[test]
    fn complete_offers_prefixes_and_flags() {
        let (session, _buf) = empty_session();
        let all = LoadTemplate.complete(&session, "", "load_template ");
        assert!(all.contains(&"SUSE:Maintenance:".to_owned()));
        assert!(all.contains(&"-a".to_owned()));
        // Prefix filtering works.
        let filtered = LoadTemplate.complete(&session, "-k", "load_template -k");
        assert_eq!(filtered, vec!["-k".to_owned()]);
    }
}
