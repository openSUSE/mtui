//! The `openqa_overview` command — openQA / Dashboard / build-check overview.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::oqa_search as oqa;
use mtui_datasources::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_testreport::{FileList, inject_overview};

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The aggregated-update job groups offered for tab completion (upstream
/// `_AGGREGATED_GROUP_CHOICES`).
const AGGREGATED_GROUP_CHOICES: &[&str] = &["core", "containers", "yast", "security"];

/// Prints an openQA / QAM Dashboard / build-checks overview for the loaded MU.
///
/// Ports upstream `mtui.commands.openqa_overview.OpenQAOverview` (the `oqa-search`
/// helper). Fetches three sections — single incidents, aggregated updates, build
/// checks — and prints them to the REPL; `--export` also injects the plain-text
/// block into the loaded testreport's `log` under `regression tests:`.
///
/// Deviation from upstream: the `--no-fetch` cache reuse (upstream reads
/// `metadata.openqa.overview`) is deferred with the openQA state holder
/// (`mtui-rs-zs4`); passing `--no-fetch` here logs and fetches anyway.
pub struct OpenQAOverview;

#[async_trait]
impl Command for OpenQAOverview {
    fn name(&self) -> &'static str {
        "openqa_overview"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Prints an openQA / QAM Dashboard / build-checks overview for the loaded MU.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("no_aggregated")
                .long("no-aggregated")
                .action(ArgAction::SetTrue)
                .help("Skip the Aggregated Updates section"),
        )
        .arg(
            Arg::new("days")
                .long("days")
                .value_name("N")
                .value_parser(clap::value_parser!(u32).range(1..=30))
                .default_value("5")
                .help("How many days to scan back for aggregated builds (1-30, default 5)"),
        )
        .arg(
            Arg::new("aggregated_groups")
                .long("aggregated-groups")
                .value_name("GROUP")
                .action(ArgAction::Append)
                .value_parser(clap::builder::PossibleValuesParser::new(
                    AGGREGATED_GROUP_CHOICES,
                ))
                .help("Job groups to search in Aggregated Updates (default: core)"),
        )
        .arg(
            Arg::new("url_openqa")
                .long("url-openqa")
                .value_name("URL")
                .help("Override openQA URL (default: config openqa_instance)"),
        )
        .arg(
            Arg::new("url_dashboard_qam")
                .long("url-dashboard-qam")
                .value_name("URL")
                .help("Override QAM Dashboard base URL (default: derived from config)"),
        )
        .arg(
            Arg::new("url_qam")
                .long("url-qam")
                .value_name("URL")
                .help("Override QAM base URL (default: derived from config reports_url)"),
        )
        .arg(
            Arg::new("test_pattern")
                .long("test-pattern")
                .value_name("REGEX")
                .help("Custom regex to extract test results from build-check logs"),
        )
        .arg(
            Arg::new("export")
                .long("export")
                .action(ArgAction::SetTrue)
                .help("Also inject the overview into the loaded testreport's log"),
        )
        .arg(
            Arg::new("no_fetch")
                .long("no-fetch")
                .action(ArgAction::SetTrue)
                .help("(deferred) reuse cached overview; currently fetches anyway"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = [
            "--no-aggregated",
            "--days",
            "--aggregated-groups",
            "--url-openqa",
            "--url-dashboard-qam",
            "--url-qam",
            "--test-pattern",
            "--export",
            "--no-fetch",
        ]
        .iter()
        .filter(|f| f.starts_with(text))
        .map(|s| (*s).to_owned())
        .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;

        if args.get_flag("no_fetch") {
            tracing::warn!(
                "--no-fetch given but the overview cache is not yet available; fetching anyway"
            );
        }

        let no_aggregated = args.get_flag("no_aggregated");
        let days = args.get_one::<u32>("days").copied().unwrap_or(5);
        let groups: Vec<String> = args
            .get_many::<String>("aggregated_groups")
            .map(|it| it.cloned().collect())
            .unwrap_or_else(|| vec!["core".to_owned()]);

        let url_openqa = args
            .get_one::<String>("url_openqa")
            .cloned()
            .unwrap_or_else(|| session.config.openqa_instance.clone());
        let url_dashboard_qam = args
            .get_one::<String>("url_dashboard_qam")
            .cloned()
            .unwrap_or_else(|| derive_dashboard_url(&session.config.qem_dashboard_api));
        let url_qam = args
            .get_one::<String>("url_qam")
            .cloned()
            .unwrap_or_else(|| derive_qam_url(&session.config.reports_url));

        let verify = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&session.config.ssl_verify)),
        );
        let http = HttpClient::new(verify)
            .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;

        // incident_id is maintenance_id ("1.2" for SLFO); effective id falls back
        // to the review id (the Gitea PR number) when maintenance_id is non-int.
        let incident_id = rrid.maintenance_id.clone();
        let request_id = rrid.review_id;
        let product = rrid.kind.as_str().to_owned();
        let effective_incident_id = if incident_id.parse::<i64>().is_ok() {
            incident_id.clone()
        } else {
            request_id.to_string()
        };

        session.display.println(&session.display.blue("OpenQA:"));
        session.display.println(&session.display.blue("#######"));

        let (build, versions) =
            match oqa::get_incident_info(&http, &url_dashboard_qam, &effective_incident_id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Failed to query QEM Dashboard: {e}");
                    return Ok(());
                }
            };

        let mut single_incidents = Vec::new();
        let mut aggregated = Vec::new();

        if let Some(versions) = versions.as_ref().filter(|v| !v.is_empty()) {
            single_incidents = oqa::single_incidents(&http, &build, versions, &url_openqa).await;
            session
                .display
                .println(&session.display.blue("Single incidents - Core"));
            for row in &single_incidents {
                print_version_row(session, row);
            }

            if !no_aggregated {
                session.display.println("-------");
                aggregated = oqa::aggregated_updates(
                    &http,
                    &effective_incident_id,
                    versions,
                    days,
                    &groups,
                    &url_openqa,
                )
                .await;
                for group in &aggregated {
                    session.display.println(&session.display.blue(&format!(
                        "\nAggregated updates - {}",
                        title_case(&group.group)
                    )));
                    for row in &group.versions {
                        print_version_row(session, row);
                    }
                }
                if aggregated.is_empty() {
                    let msg = session
                        .display
                        .yellow("No aggregated updates builds available for this incident");
                    session.display.println(&msg);
                }
            }
        } else {
            let msg = session
                .display
                .yellow("No openQA builds for this incident yet");
            session.display.println(&msg);
        }

        session.display.println("-------");
        session
            .display
            .println(&session.display.blue("\nBuild checks:"));
        session
            .display
            .println(&session.display.blue("#############"));

        let mut packages = session.metadata().get_package_list();
        if packages.is_empty()
            && !build.is_empty()
            && let Some(last) = build.rsplit(':').next()
        {
            packages = vec![last.to_owned()];
        }
        let build_checks = oqa::build_checks(
            &http,
            &product,
            &incident_id,
            i64::try_from(request_id).unwrap_or(0),
            &packages,
            &url_qam,
            args.get_one::<String>("test_pattern").map(String::as_str),
        )
        .await;
        if build_checks.is_empty() {
            session.display.println("No build checks for this incident");
        } else {
            for entry in &build_checks {
                print_build_check(session, entry);
            }
        }

        if args.get_flag("export") {
            export_to_testreport(
                session,
                &single_incidents,
                &aggregated,
                &build_checks,
                no_aggregated,
            );
        }
        Ok(())
    }
}

/// Injects the overview block into the loaded testreport `log` (upstream
/// `_export_to_testreport`).
fn export_to_testreport(
    session: &mut Session,
    single_incidents: &[oqa::VersionResult],
    aggregated: &[oqa::GroupResult],
    build_checks: &[oqa::BuildCheckResult],
    no_aggregated: bool,
) {
    let Some(path) = session.metadata().base().path.clone() else {
        tracing::error!("No testreport path available; cannot export");
        return;
    };
    let mut file = match FileList::load(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Could not read testreport {}: {e}", path.display());
            return;
        }
    };
    let modified = inject_overview(
        &mut file,
        single_incidents,
        aggregated,
        build_checks,
        no_aggregated,
    );
    // `FileList` derefs to `Vec<String>`, which `inject_overview` mutates in place.
    if modified {
        if let Err(e) = file.write() {
            tracing::error!("Failed to write overview to {}: {e}", path.display());
            return;
        }
        tracing::info!("openqa_overview block written to {}", path.display());
    } else {
        tracing::warn!(
            "Could not locate 'regression tests:' section in {}; overview NOT exported",
            path.display()
        );
    }
}

/// Drops a trailing `/api` to recover the Dashboard base URL (upstream
/// `_derive_dashboard_url`).
fn derive_dashboard_url(qem_dashboard_api: &str) -> String {
    qem_dashboard_api
        .trim_end_matches('/')
        .strip_suffix("/api")
        .unwrap_or(qem_dashboard_api.trim_end_matches('/'))
        .to_owned()
}

/// Drops a trailing `/testreports` to recover the QAM base URL (upstream
/// `_derive_qam_url`).
fn derive_qam_url(reports_url: &str) -> String {
    reports_url
        .trim_end_matches('/')
        .strip_suffix("/testreports")
        .unwrap_or(reports_url.trim_end_matches('/'))
        .to_owned()
}

/// Title-cases each whitespace-separated word (upstream `str.title()`, scoped to
/// the simple group names used here).
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prints one PASSED/FAILED/RUNNING/MISSING row (upstream `_print_version_row`).
///
/// Mirrors upstream: the `version -> url` (or bare `version`) line, then the
/// status label colored by state — `FAILED (n jobs)` red, `RUNNING/SCHEDULED
/// (n jobs)` yellow, `PASSED` green — then the optional note in yellow.
fn print_version_row(session: &mut Session, row: &oqa::VersionResult) {
    if row.status == "missing" {
        let msg = session
            .display
            .yellow(&format!("{} -> {}", row.version, row.note));
        session.display.println(&msg);
        return;
    }
    if row.url.is_empty() {
        session.display.println(&row.version);
    } else {
        session
            .display
            .println(&format!("{} -> {}", row.version, row.url));
    }

    let label = match row.status.as_str() {
        "failed" => {
            let text = if row.failed_count != 0 {
                format!("FAILED ({} jobs)", row.failed_count)
            } else {
                "FAILED".to_owned()
            };
            session.display.red(&text)
        }
        "running" => {
            let text = if row.running_count != 0 {
                format!("RUNNING/SCHEDULED ({} jobs)", row.running_count)
            } else {
                "RUNNING/SCHEDULED".to_owned()
            };
            session.display.yellow(&text)
        }
        _ => session.display.green("PASSED"),
    };
    session.display.println(&label);

    if !row.note.is_empty() {
        let note = session.display.yellow(&row.note);
        session.display.println(&note);
    }
}

/// Prints one build-check entry (upstream `_print_build_check`).
fn print_build_check(session: &mut Session, entry: &oqa::BuildCheckResult) {
    session.display.println(&entry.url);
    for line in &entry.matches {
        session.display.println(&format!("  {line}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(OpenQAOverview.name(), "openqa_overview");
        assert_eq!(OpenQAOverview.scope(), Scope::Fanout);
    }

    #[test]
    fn url_derivation_strips_suffixes() {
        assert_eq!(
            derive_dashboard_url("http://dashboard.qam.suse.de/api"),
            "http://dashboard.qam.suse.de"
        );
        assert_eq!(
            derive_qam_url("https://qam.suse.de/testreports"),
            "https://qam.suse.de"
        );
        // No suffix → unchanged (minus trailing slash).
        assert_eq!(derive_dashboard_url("http://x/"), "http://x");
    }

    #[test]
    fn title_case_capitalizes_words() {
        assert_eq!(title_case("core"), "Core");
        assert_eq!(title_case("yast security"), "Yast Security");
        assert_eq!(title_case(""), "");
    }

    fn version_row(
        status: &str,
        url: &str,
        failed: usize,
        running: usize,
        note: &str,
    ) -> oqa::VersionResult {
        oqa::VersionResult {
            version: "15-SP5".to_owned(),
            url: url.to_owned(),
            status: status.to_owned(),
            failed_count: failed,
            running_count: running,
            note: note.to_owned(),
        }
    }

    #[test]
    fn version_row_failed_prints_red_label_and_note() {
        let (mut session, buf) = empty_session();
        print_version_row(
            &mut session,
            &version_row("failed", "http://oqa/x", 3, 0, "flaky"),
        );
        let out = buf.contents();
        assert!(out.contains("15-SP5 -> http://oqa/x"));
        assert!(out.contains("FAILED (3 jobs)"));
        assert!(out.contains("flaky"));
    }

    #[test]
    fn version_row_failed_without_count_omits_parenthetical() {
        let (mut session, buf) = empty_session();
        print_version_row(&mut session, &version_row("failed", "", 0, 0, ""));
        let out = buf.contents();
        // No url -> bare version line (upstream parity, not "version -> note").
        assert!(out.contains("15-SP5\n"));
        assert!(out.contains("FAILED\n"));
        assert!(!out.contains("FAILED ("));
    }

    #[test]
    fn version_row_running_prints_yellow_label() {
        let (mut session, buf) = empty_session();
        print_version_row(&mut session, &version_row("running", "", 0, 2, ""));
        let out = buf.contents();
        assert!(out.contains("RUNNING/SCHEDULED (2 jobs)"));
    }

    #[test]
    fn version_row_passed_prints_passed() {
        let (mut session, buf) = empty_session();
        print_version_row(
            &mut session,
            &version_row("passed", "http://oqa/ok", 0, 0, ""),
        );
        let out = buf.contents();
        assert!(out.contains("PASSED"));
        assert!(!out.contains("FAILED"));
    }

    #[test]
    fn version_row_missing_is_unchanged() {
        let (mut session, buf) = empty_session();
        print_version_row(&mut session, &version_row("missing", "", 0, 0, "no build"));
        let out = buf.contents();
        assert!(out.contains("15-SP5 -> no build"));
        // Missing rows never emit a status label.
        assert!(!out.contains("PASSED"));
        assert!(!out.contains("FAILED"));
    }

    #[test]
    fn version_row_labels_are_colored_under_always() {
        use crate::commands::testkit::Buffer;
        use crate::display::{ColorMode, CommandPromptDisplay};

        let buf = Buffer::new();
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Always);
        let mut session =
            crate::Session::with_display(mtui_config::Config::default(), false, display);

        print_version_row(&mut session, &version_row("failed", "", 1, 0, ""));
        let out = buf.contents();
        // Red ANSI escape wraps the FAILED label.
        assert!(out.contains('\u{1b}'), "expected ANSI escape, got: {out:?}");
        assert!(out.contains("FAILED (1 jobs)"));
    }

    #[test]
    fn aggregated_groups_reject_unknown_choice() {
        let base = clap::Command::new("openqa_overview").no_binary_name(true);
        let cmd = OpenQAOverview.configure(base);
        assert!(
            cmd.clone()
                .try_get_matches_from(["--aggregated-groups", "bogus"])
                .is_err()
        );
        assert!(
            cmd.try_get_matches_from(["--aggregated-groups", "core"])
                .is_ok()
        );
    }

    #[test]
    fn days_out_of_range_rejected() {
        let base = clap::Command::new("openqa_overview").no_binary_name(true);
        let cmd = OpenQAOverview.configure(base);
        assert!(cmd.clone().try_get_matches_from(["--days", "0"]).is_err());
        assert!(cmd.clone().try_get_matches_from(["--days", "31"]).is_err());
        assert!(cmd.try_get_matches_from(["--days", "5"]).is_ok());
    }

    #[test]
    fn completion_offers_flags_and_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = OpenQAOverview.complete(&session, "--no", "");
        assert!(out.iter().any(|c| c == "--no-aggregated"));
        assert!(out.iter().any(|c| c == "--no-fetch"));
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&OpenQAOverview, &[]);
        let err = OpenQAOverview.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn full_fetch_renders_sections_and_exports() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Dashboard incident_settings → a build + one version.
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/incident_settings/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"settings": {"BUILD": "20260101-1", "DISTRI": "sle", "VERSION": "15-SP5"}}
            ])))
            .mount(&server)
            .await;
        // openQA job groups + jobs: return empty groups/jobs so single_incidents
        // yields rows without failing, and build_checks index 404s (no checks).
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "JobGroups": [], "jobs": []
            })))
            .mount(&server)
            .await;

        // A loaded report whose `log` has a regression-tests section for export.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(
            &log,
            "comment: hi\n\nregression tests:\n-----------------\n\n",
        )
        .unwrap();

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().path = Some(log.clone());

        let args = matches(
            &OpenQAOverview,
            &[
                "--export",
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
                "--url-qam",
                &server.uri(),
            ],
        );
        OpenQAOverview.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Single incidents - Core"), "{out}");
        assert!(out.contains("Build checks:"), "{out}");
        // The export path wrote the overview block into the log.
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("OpenQA Overview"), "{written}");
    }

    #[tokio::test]
    async fn export_without_report_path_is_a_noop_not_a_panic() {
        // No path on the report → export logs an error and returns; command Ok.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // Dashboard 500 → returns before export, but exercise export helper too.
        export_to_testreport(&mut session, &[], &[], &[], false);
    }

    #[tokio::test]
    async fn dashboard_unreachable_prints_header_and_returns_ok() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Any dashboard call 500s → get_incident_info errors → command logs and
        // returns Ok after printing the header.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAOverview,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        OpenQAOverview.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("OpenQA:"));
    }
}
