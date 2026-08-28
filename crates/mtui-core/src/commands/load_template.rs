//! The `load_template` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use mtui_testreport::UpdateKind;
use mtui_types::UpdateID;

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Loads a maintenance-update template into the session and connects its
/// reference hosts.
///
/// Exactly one of `-a`/`--auto-review-id` (an automatic OBS update) or
/// `-k`/`--kernel-review-id` (a kernel/live-patch update) is required. The
/// template is **added** to the registry (keyed by RRID) and made active;
/// re-loading an already-loaded RRID replaces its stored report and re-activates
/// it, leaving siblings untouched.
///
/// It names its own target RRID and connects only that template's reference
/// hosts, so it runs once ([`Scope::Single`]) however many are loaded —
/// otherwise it would fan out under MCP and re-run the autoconnect, grabbing
/// pool hosts, on every one. `-a` autoconnects; `-k` starts the kernel workflow
/// and does not ([`Session::load_update`] honours that intent).
///
/// `--force-continue` is the non-interactive escape hatch for a stale
/// template hash (openSUSE/mtui#517): the interactive REPL already offers
/// this as its "Force continue loading template ?" fallback prompt, but that
/// prompt is unreachable for any non-interactive caller (every `mtui-mcp`
/// session), so a template TeReGen also refuses to regenerate (already
/// hand-edited) was permanently unloadable outside the REPL. The flag reaches
/// exactly the same outcome the REPL's own "y" answer does — load the
/// existing checked-out report as-is — and nothing else; it does not affect
/// the earlier "Regenerate via TeReGen?" question, which stays
/// interactive-only (`regenerate` is the dedicated non-interactive tool for
/// that).
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

    fn reads_resolved_report(&self) -> bool {
        // Reads the report it just loaded, never the one it was handed.
        false
    }

    fn requires_canonical_session(&self, _argv: &[String]) -> bool {
        true
    }

    /// The newly loaded template must become active unconditionally, even with
    /// another one already active.
    fn repoints_active(&self) -> bool {
        true
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
        // Mutually exclusive, and exactly one is required.
        .group(
            ArgGroup::new("review_id")
                .args(["auto", "kernel"])
                .required(true)
                .multiple(false),
        )
        .arg(
            Arg::new("force_continue")
                .long("force-continue")
                .action(ArgAction::SetTrue)
                .help(
                    "Load a stale checked-out template as-is, instead of aborting, once \
                     TeReGen has refused to regenerate it non-interactively.",
                ),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        [
            "SUSE:Maintenance:",
            "openSUSE:Maintenance:",
            "-a",
            "--auto-review-id",
            "-k",
            "--kernel-review-id",
            "--force-continue",
        ]
        .into_iter()
        .filter(|c| c.starts_with(text))
        .map(str::to_owned)
        .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
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

        let force_continue = args.get_flag("force_continue");

        // Autoconnect is always *requested*; the update kind decides whether a
        // connect actually happens.
        let (loaded, reason) = session
            .load_update_reported(&update, true, kind, force_continue)
            .await;
        if loaded.is_empty() {
            return Err(CommandError::Other(match reason {
                Some(why) => format!("could not load template for {rrid}: {why}"),
                None => format!("could not load template for {rrid}"),
            }));
        }
        let connected = session.targets().len();
        // Surface a force-continued stale hash in the tool result, not just
        // `tracing::warn!` (never seen under mtui-mcp). Cloned first: holding
        // `session.metadata()`'s borrow live would collide with `println`'s
        // `&mut session.display` below.
        let stale_warning = session.metadata().base().stale_hash_warning.clone();
        if let Some(warning) = stale_warning {
            session.display.println(&format!("warning: {warning}"));
        }
        session
            .display
            .println(&format!("loaded {rrid} ({connected} hosts connected)"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(LoadTemplate.name(), "load_template");
        assert_eq!(LoadTemplate.scope(), Scope::Single);
    }

    #[test]
    fn requires_exactly_one_review_id() {
        let cmd = LoadTemplate.configure(clap::Command::new("load_template"));
        assert!(cmd.clone().try_get_matches_from(["load_template"]).is_err());
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
        // An un-checkout-able RRID must error, not register a phantom.
        let (mut session, _buf) = empty_session();
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&LoadTemplate, &["-k", "SUSE:Maintenance:1:1"]);
        let err = LoadTemplate.call(&mut session, &args).await.unwrap_err();
        // The threaded-through cause, so the operator sees *why* it failed.
        assert!(
            matches!(&err, CommandError::Other(m)
            if m.contains("could not load") && m.contains("svn checkout")),
            "{err:?}"
        );
        assert!(session.templates.is_empty());
    }

    #[tokio::test]
    async fn kernel_load_registers_and_activates() {
        // Kernel does not autoconnect, so this registers and activates only.
        let (mut session, buf) = empty_session();
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
        // A success line reaches the display so the MCP result is never empty.
        let out = buf.contents();
        assert!(out.contains(&format!("loaded {rrid}")), "{out:?}");
        assert!(out.contains("hosts connected"), "{out:?}");
    }

    /// Through `Command::run`, not `call` — `run` is what restores the
    /// pre-dispatch pointer, so only driving it this way can see the revert
    /// `call`-only tests can't.
    #[tokio::test]
    async fn load_over_a_prior_active_template_activates_the_new_one() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
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
        LoadTemplate.run(&mut session, &args).await.unwrap();

        assert_eq!(session.templates.active_rrid(), Some(rrid));
    }

    /// The opt-out does not strand the pointer when `load_update_reported`
    /// never moved it: an unloadable RRID leaves the prior template active.
    #[tokio::test]
    async fn failed_load_keeps_the_prior_active_template() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let tmp = tempfile::tempdir().unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.svn_path = format!("file://{}/no-repo", tmp.path().display());

        let args = matches(&LoadTemplate, &["-k", "SUSE:Maintenance:2:2"]);
        let err = LoadTemplate.run(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:1:1")
        );
    }

    #[test]
    fn complete_offers_prefixes_and_flags() {
        let (session, _buf) = empty_session();
        let all = LoadTemplate.complete(&session, "", "load_template ");
        assert!(all.contains(&"SUSE:Maintenance:".to_owned()));
        assert!(all.contains(&"-a".to_owned()));
        assert!(all.contains(&"--force-continue".to_owned()));
        // Prefix filtering works.
        let filtered = LoadTemplate.complete(&session, "-k", "load_template -k");
        assert_eq!(filtered, vec!["-k".to_owned()]);
    }

    /// `--force-continue` defaults to unset and parses as a plain boolean flag
    /// alongside the required `-a`/`-k` group.
    #[test]
    fn force_continue_flag_defaults_false_and_parses() {
        let cmd = LoadTemplate.configure(clap::Command::new("load_template"));
        let without = cmd
            .clone()
            .try_get_matches_from(["load_template", "-a", "SUSE:Maintenance:1:1"])
            .unwrap();
        assert!(!without.get_flag("force_continue"));

        let with = cmd
            .try_get_matches_from([
                "load_template",
                "-a",
                "SUSE:Maintenance:1:1",
                "--force-continue",
            ])
            .unwrap();
        assert!(with.get_flag("force_continue"));
    }

    /// End-to-end: `--force-continue` reaches all the way from the parsed
    /// clap flag through `Session::load_update_reported` to
    /// `make_testreport`/`handle_stale_hash` and actually loads a
    /// stale-hash SLFO template that would otherwise abandon the load — the
    /// same shape `kernel_load_registers_and_activates` proves for `-k`
    /// alone, but for the new flag specifically. Uses `-k`
    /// (not `-a`) to skip the QEM-dashboard auto-openQA enrichment `-a`
    /// would trigger on a successful load — `tr_factory` selects the report
    /// class (and hence `check_hash`'s real Gitea comparison) from the RRID
    /// kind, not from `-a`/`-k`, so the stale-hash path is identical either
    /// way. Also pins that the force-continued load prints a `warning:` line
    /// to the command's display output — the tool-result-visibility half of
    /// the fix, not just that the load succeeds.
    #[tokio::test]
    async fn force_continue_flag_loads_stale_edited_template() {
        let gitea = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "head": { "sha": "freshsha" } })),
            )
            .mount(&gitea)
            .await;

        let (mut session, buf) = empty_session();
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:SLFO:1.2:4413";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            serde_json::json!({
                "rrid": rrid,
                "repository": "http://download.suse.de/ibs/SUSE:/SLFO:/1.2/",
                "products": ["SLES 16.0 (x86_64)"],
                "gitea_pr_api": format!("{}/pulls/1", gitea.uri()),
                // Differs from the mocked PR head ("freshsha") — the hash
                // mismatch this whole path exists to force-continue past.
                "gitea_commit_hash": "stalesha",
                "packages": {},
                "testplatform": [],
            })
            .to_string(),
        )
        .unwrap();
        session.config.template_dir = tmp.path().to_path_buf();
        session.config.gitea_token = "tok".to_owned();
        session.config.gitea_url = gitea.uri();

        let args = matches(&LoadTemplate, &["-k", rrid, "--force-continue"]);
        LoadTemplate.call(&mut session, &args).await.unwrap();

        assert!(
            session.templates.contains(rrid),
            "a stale-but-force-continued template must register"
        );
        assert_eq!(session.templates.active_rrid(), Some(rrid));
        let out = buf.contents();
        assert!(
            out.contains("warning:") && out.contains("stale checkout"),
            "the tool result must flag the stale hash, not just silently load: {out:?}"
        );
        assert!(out.contains(&format!("loaded {rrid}")), "{out:?}");
    }
}
