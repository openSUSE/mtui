//! The `show_update_repos` command.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use futures::StreamExt;
use mtui_datasources::HttpClient;
use mtui_datasources::repo_check::{Listed, NoAnswer, RepoCheck, RepoStatus, check_repository};
use mtui_types::SystemProduct;
use mtui_types::report_document::ReportDocument;
use tokio_util::sync::CancellationToken;

use super::support::{one_line, session_http, unless_cancelled};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Repositories checked at once.
const CHECK_CONCURRENCY: usize = 4;
/// Wall-clock budget for the whole `--check`.
const CHECK_BUDGET: Duration = Duration::from_secs(300);
/// The longest reason or name printed, in characters.
const TEXT_LIMIT: usize = 2000;

/// Shows the update repositories valid for the current update, from the active
/// report's [`update_repos`](mtui_testreport::TestReportBase::update_repos).
pub struct ShowUpdateRepos;

/// Where the repositories `--check` covers came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The document's `install.targets[]`, with their binaries.
    Targets,
    /// The report's `update_repos`: no binaries are known.
    UpdateRepos,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Targets => "install.targets",
            Self::UpdateRepos => "update_repos",
        }
    }
}

/// One repository to check, under the product its first target names.
#[derive(Debug, Clone)]
struct Repo {
    product: SystemProduct,
    url: String,
    listed: Listed,
}

/// A checked repository, ready to print.
#[derive(Debug, Clone)]
pub(crate) struct CheckedRow {
    /// The product as the document names it, made printable.
    pub(crate) product: SystemProduct,
    /// The repository URL, credentials masked.
    pub(crate) repository: String,
    /// The verdict.
    pub(crate) check: RepoCheck,
}

#[async_trait]
impl Command for ShowUpdateRepos {
    fn name(&self) -> &'static str {
        "show_update_repos"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Shows the update repositories that are valid for the current update.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn invocation_scope(&self, args: &ArgMatches) -> Result<Scope, CommandError> {
        if !args.get_flag("json") {
            return Ok(self.scope());
        }
        if args.try_get_one::<bool>("all_templates").ok().flatten() == Some(&true) {
            return Err(CommandError::Other(
                "--json reports one template; drop --all-templates and pick one with -T".to_owned(),
            ));
        }
        // One JSON document per call: never fan out implicitly.
        Ok(Scope::Explicit)
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue)
                .help(
                    "fetch each install target's repository (repodata/repomd.xml and its \
                     primary metadata) and report it as ok, missing (listed binaries not at \
                     their version-release.arch), empty, not published (repomd.xml 404) or no \
                     answer (any other failure); fails when any is not ok or none could be \
                     checked, after listing all",
                ),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .requires("check")
                .help(
                    "with --check, print one JSON object {source, total, checked, not_ok, \
                     ok, repositories: [{product, version, arch, repository, status, \
                     packages, missing, reason}]} for one template: with --json the command \
                     never fans out, so --all-templates is refused and, with several templates \
                     loaded, -T picks one",
                ),
        )
    }

    fn complete(&self, _session: &Session, text: &str, line: &str) -> Vec<String> {
        super::support::complete_choices(&[&["--check"], &["--json"]], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        if args.get_flag("check") {
            return self.check(session, args.get_flag("json")).await;
        }
        // Snapshotted so the display's mutable borrow does not overlap the
        // report's immutable one.
        let mut repos: Vec<(SystemProduct, String)> = session
            .metadata()
            .base()
            .update_repos
            .iter()
            .map(|(p, r)| (p.clone(), r.clone()))
            .collect();
        repos.sort_by(|a, b| a.0.name.cmp(&b.0.name).then(a.0.version.cmp(&b.0.version)));
        if repos.is_empty() {
            session.display.println("No update repos for this update.");
            return Ok(());
        }
        session.display.list_update_repos(&repos);
        Ok(())
    }
}

impl ShowUpdateRepos {
    async fn check(&self, session: &mut Session, as_json: bool) -> CommandResult {
        let base = session.metadata().base();
        let (source, repos) = repositories(base.document.as_ref(), &base.update_repos);
        let total = repos.len();
        if total == 0 {
            if as_json {
                session.display.println(&summary_json(source, 0, &[]));
            }
            return Err(CommandError::Other(
                "--check found no repository to check: the report lists no install targets \
                 and no update repositories"
                    .to_owned(),
            ));
        }
        let http = match session_http(session) {
            Ok(http) => http,
            Err(e) => {
                if as_json {
                    session.display.println(&summary_json(source, total, &[]));
                }
                return Err(e);
            }
        };
        let cancel = session.cancel_token();
        let work = repos.iter().map(|r| (r.url.clone(), r.listed.clone()));
        let results = check_all(&http, work.collect(), CHECK_BUDGET, &cancel).await;

        let rows: Vec<CheckedRow> = repos
            .iter()
            .zip(results)
            .filter_map(|(repo, result)| {
                result.map(|check| CheckedRow {
                    product: SystemProduct::new(
                        one_line(&repo.product.name, Some(TEXT_LIMIT)),
                        one_line(&repo.product.version, Some(TEXT_LIMIT)),
                        one_line(&repo.product.arch, Some(TEXT_LIMIT)),
                    ),
                    repository: one_line(&mtui_datasources::sanitize_url(&repo.url), None),
                    check,
                })
            })
            .collect();
        if as_json {
            session.display.println(&summary_json(source, total, &rows));
        } else {
            if source == Source::UpdateRepos {
                session.display.println(
                    "The report lists no install targets: each update repository is checked \
                     for being published and non-empty only.",
                );
            }
            session.display.list_update_repo_checks(&rows);
        }

        // A repository found not ok before a cancel outranks the cancel.
        let failing = not_ok(&rows);
        let stopped = if rows.len() < total {
            format!("; stopped after {} of {total}", rows.len())
        } else {
            String::new()
        };
        if failing > 0 {
            return Err(CommandError::Other(format!(
                "{failing} of {total} update repositories are not ok{stopped}"
            )));
        }
        if rows.len() < total {
            return Err(CommandError::Cancelled(format!(
                "checked {} of {total} update repositories",
                rows.len()
            )));
        }
        Ok(())
    }
}

/// A repository URL as matched between targets.
fn repo_key(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// The repositories to check, sorted: one per distinct `install.targets[]`
/// repository with the binaries every target naming it lists, or, when the
/// document has no targets, the `update_repos` with none listed.
fn repositories(
    doc: Option<&ReportDocument>,
    update_repos: &std::collections::HashMap<SystemProduct, String>,
) -> (Source, Vec<Repo>) {
    let mut by_url: BTreeMap<&str, Repo> = BTreeMap::new();
    for target in doc.iter().flat_map(|d| d.install.targets.iter()) {
        by_url
            .entry(repo_key(&target.repository))
            .or_insert_with(|| Repo {
                product: SystemProduct::new(&target.product, &target.version, &target.arch),
                url: target.repository.clone(),
                listed: Listed::new(),
            })
            .listed
            .extend(target.binaries.clone());
    }
    let (source, mut repos): (Source, Vec<Repo>) = if by_url.is_empty() {
        // Ordered, so a repository two products name keeps the same one every run.
        let mut entries: Vec<(&SystemProduct, &String)> = update_repos.iter().collect();
        entries.sort();
        let mut seen = std::collections::BTreeSet::new();
        let repos = entries
            .into_iter()
            .filter(|(_, url)| seen.insert(repo_key(url).to_owned()))
            .map(|(product, url)| Repo {
                product: product.clone(),
                url: url.clone(),
                listed: Listed::new(),
            })
            .collect();
        (Source::UpdateRepos, repos)
    } else {
        (Source::Targets, by_url.into_values().collect())
    };
    repos.sort_by(|a, b| {
        (&a.product.name, &a.product.version, &a.product.arch, &a.url).cmp(&(
            &b.product.name,
            &b.product.version,
            &b.product.arch,
            &b.url,
        ))
    });
    (source, repos)
}

/// Checks every repository, [`CHECK_CONCURRENCY`] at a time, in order. A
/// repository unfinished when `budget` runs out is `no answer`; one not
/// finished when `cancel` fires is `None`.
async fn check_all(
    http: &HttpClient,
    work: Vec<(String, Listed)>,
    budget: Duration,
    cancel: &CancellationToken,
) -> Vec<Option<RepoCheck>> {
    let deadline = tokio::time::Instant::now() + budget;
    futures::stream::iter(work)
        .map(|(url, listed)| async move {
            let check = async {
                tokio::time::timeout_at(deadline, check_repository(http, &url, &listed))
                    .await
                    .unwrap_or(RepoCheck::NoAnswer(NoAnswer::OutOfTime(budget)))
            };
            unless_cancelled(cancel, check).await
        })
        .buffered(CHECK_CONCURRENCY)
        .collect()
        .await
}

fn not_ok(rows: &[CheckedRow]) -> usize {
    rows.iter()
        .filter(|r| r.check.status() != RepoStatus::Ok)
        .count()
}

/// The `--json` document: the verdict summary and one entry per checked row.
fn summary_json(source: Source, total: usize, rows: &[CheckedRow]) -> String {
    let failing = not_ok(rows);
    let list: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let missing: Vec<String> = row
                .check
                .missing()
                .iter()
                .map(|m| one_line(m, Some(TEXT_LIMIT)))
                .collect();
            serde_json::json!({
                "product": row.product.name,
                "version": row.product.version,
                "arch": row.product.arch,
                "repository": row.repository,
                "status": row.check.status().as_str(),
                "packages": row.check.packages(),
                "missing": missing,
                "reason": one_line(&row.check.to_string(), Some(TEXT_LIMIT)),
            })
        })
        .collect();
    serde_json::json!({
        "source": source.as_str(),
        "total": total,
        "checked": rows.len(),
        "not_ok": failing,
        "ok": total > 0 && rows.len() == total && failing == 0,
        "repositories": list,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::Buffer;
    use crate::commands::testkit::teregen::minimal_document;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};
    use mtui_types::report_document::Target;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RRID: &str = "SUSE:Maintenance:1:1";
    const RRID_B: &str = "SUSE:Maintenance:2:2";

    fn primary(packages: &[(&str, &str)]) -> String {
        let mut out = format!(r#"<metadata packages="{}">"#, packages.len());
        for (name, vra) in packages {
            let (vr, arch) = vra.rsplit_once('.').unwrap();
            let (ver, rel) = vr.rsplit_once('-').unwrap();
            out.push_str(&format!(
                r#"<package type="rpm"><name>{name}</name><arch>{arch}</arch><version epoch="0" ver="{ver}" rel="{rel}"/></package>"#
            ));
        }
        out + "</metadata>"
    }

    /// Serves an rpm-md repository at `/{dir}/` with an uncompressed primary,
    /// its `repomd.xml` answering after `delay`.
    async fn serve_repo_after(
        server: &MockServer,
        dir: &str,
        packages: &[(&str, &str)],
        delay: Duration,
    ) {
        let repomd = r#"<repomd><data type="primary"><location href="repodata/primary.xml"/></data></repomd>"#;
        Mock::given(method("GET"))
            .and(path(format!("/{dir}/repodata/repomd.xml")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(repomd)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{dir}/repodata/primary.xml")))
            .respond_with(ResponseTemplate::new(200).set_body_string(primary(packages)))
            .mount(server)
            .await;
    }

    async fn serve_repo(server: &MockServer, dir: &str, packages: &[(&str, &str)]) {
        serve_repo_after(server, dir, packages, Duration::ZERO).await;
    }

    fn target(name: &str, version: &str, repository: &str, binaries: &[(&str, &str)]) -> Target {
        Target {
            product: name.to_owned(),
            version: version.to_owned(),
            arch: "x86_64".to_owned(),
            repository: repository.to_owned(),
            binaries: binaries
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// A loaded report with no host connected, whose document carries
    /// `targets` (`None`: no document at all) and whose `update_repos` is
    /// `update_repos`.
    fn session_with(
        targets: Option<Vec<Target>>,
        update_repos: &[(SystemProduct, String)],
    ) -> (Session, Buffer) {
        let (mut session, buf) = session_with_hosts(RRID, &[], "ok");
        let base = session.metadata_mut().base_mut();
        base.document = targets.map(|targets| {
            let mut doc = minimal_document(RRID);
            doc.install.targets = targets;
            doc
        });
        base.update_repos = update_repos.iter().cloned().collect();
        (session, buf)
    }

    /// The output with the mock server's address replaced by a fixed one.
    fn stable(out: &str, server: &MockServer) -> String {
        out.replace(&server.address().to_string(), "repo.invalid")
    }

    const LISTED: &[(&str, &str)] = &[("foo", "1.2-3.1.x86_64"), ("foo-doc", "1.2-3.1.noarch")];

    #[tokio::test]
    async fn check_reports_every_repository_and_fails_when_any_is_not_ok() {
        let server = MockServer::start().await;
        serve_repo(&server, "ok", LISTED).await;
        serve_repo(&server, "missing", &[("foo", "1.2-2.1.x86_64")]).await;
        serve_repo(&server, "empty", &[]).await;
        Mock::given(method("GET"))
            .and(path("/broken/repodata/repomd.xml"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let uri = server.uri();
        let with_creds = uri.replace("http://", "http://alice:s3cret@");
        // A name carrying terminal control sequences, as a hostile report could.
        let hostile: &[(&str, &str)] = &[
            ("foo", "1.2-3.1.x86_64"),
            ("evil\u{1b}[2J\u{1b}]52;c;x\u{7}name", "1-1.x86_64"),
        ];
        let (mut session, buf) = session_with(
            Some(vec![
                target("SLES", "15.5", &format!("{with_creds}/ok/"), LISTED),
                target("SLES", "16.0", &format!("{uri}/missing"), hostile),
                target("SLES", "16.1", &format!("{uri}/gone"), LISTED),
                target("SL-Micro", "6.0", &format!("{uri}/empty"), LISTED),
                target("SL-Micro", "6.1", &format!("{uri}/broken"), LISTED),
            ]),
            &[],
        );

        let args = matches(&ShowUpdateRepos, &["--check"]);
        let err = ShowUpdateRepos
            .call(&mut session, &args)
            .await
            .expect_err("a repository that is not ok fails the command");

        assert_eq!(err.to_string(), "4 of 5 update repositories are not ok");
        let out = buf.contents();
        assert!(!out.contains("s3cret") && !out.contains("alice"), "{out}");
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
        insta::assert_snapshot!(stable(&out, &server));
    }

    #[tokio::test]
    async fn every_target_repository_is_checked_under_its_document_product() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("foo", "1.2-3.1.x86_64")]).await;
        serve_repo(&server, "b", &[("bar", "2-1.x86_64")]).await;
        let uri = server.uri();
        // update_repos knows one of the two repositories, under a normalised
        // product; the check must still cover both, named as the document names them.
        let (mut session, buf) = session_with(
            Some(vec![
                target(
                    "SLES",
                    "15.5",
                    &format!("{uri}/a"),
                    &[("foo", "1.2-3.1.x86_64")],
                ),
                target(
                    "SLES",
                    "15.5",
                    &format!("{uri}/b"),
                    &[("bar", "2-1.x86_64")],
                ),
            ]),
            &[(
                SystemProduct::new("sles", "15.5", "x86_64"),
                format!("{uri}/a"),
            )],
        );

        let args = matches(&ShowUpdateRepos, &["--check", "--json"]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(buf.contents().trim()).unwrap();
        let rows = out["repositories"].as_array().unwrap();
        let urls: Vec<&str> = rows
            .iter()
            .map(|r| r["repository"].as_str().unwrap())
            .collect();
        assert_eq!(urls, [format!("{uri}/a"), format!("{uri}/b")]);
        assert!(rows.iter().all(|r| r["product"] == "SLES"), "{out}");
        assert_eq!(out["source"], "install.targets");
    }

    #[tokio::test]
    async fn without_targets_the_update_repos_are_checked_for_packages_only() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("bar", "1-1.x86_64")]).await;
        let (mut session, buf) = session_with(
            None,
            &[(
                SystemProduct::new("SLES", "15", "x86_64"),
                format!("{}/a", server.uri()),
            )],
        );

        let args = matches(&ShowUpdateRepos, &["--check"]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();
        insta::assert_snapshot!(stable(&buf.contents(), &server));
    }

    #[test]
    fn two_products_naming_one_update_repository_always_give_the_same_row() {
        let url = "http://repo.invalid/a".to_owned();
        // Each map gets its own random hasher seed, so iteration order varies.
        let rows: std::collections::BTreeSet<String> = (0..64)
            .map(|_| {
                let map: std::collections::HashMap<SystemProduct, String> = [
                    (SystemProduct::new("SLES", "15.5", "x86_64"), url.clone()),
                    (
                        SystemProduct::new("Basesystem", "15.5", "x86_64"),
                        url.clone(),
                    ),
                ]
                .into_iter()
                .collect();
                let (source, repos) = repositories(None, &map);
                assert_eq!((source, repos.len()), (Source::UpdateRepos, 1));
                repos[0].product.name.clone()
            })
            .collect();
        assert_eq!(
            rows,
            std::collections::BTreeSet::from(["Basesystem".to_owned()])
        );
    }

    #[tokio::test]
    async fn check_with_nothing_to_check_fails() {
        let (mut session, buf) = empty_session();
        let args = matches(&ShowUpdateRepos, &["--check"]);
        let err = ShowUpdateRepos.call(&mut session, &args).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "--check found no repository to check: the report lists no install targets and \
             no update repositories"
        );
        assert_eq!(buf.contents(), "");
    }

    #[tokio::test]
    async fn json_check_with_nothing_to_check_prints_a_failed_summary_and_fails() {
        let (mut session, buf) = empty_session();
        let args = matches(&ShowUpdateRepos, &["--check", "--json"]);
        ShowUpdateRepos
            .call(&mut session, &args)
            .await
            .expect_err("nothing checked is a failure");
        let out: serde_json::Value = serde_json::from_str(buf.contents().trim()).unwrap();
        assert_eq!(
            out,
            serde_json::json!({
                "source": "update_repos", "total": 0, "checked": 0, "not_ok": 0,
                "ok": false, "repositories": []
            })
        );
    }

    #[tokio::test]
    async fn json_summary_carries_the_verdict() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("foo", "1.2-3.1.x86_64")]).await;
        serve_repo(
            &server,
            "b",
            &[("bar", "2-1.noarch"), ("baz", "1-1.x86_64")],
        )
        .await;
        let uri = server.uri();
        let (mut session, buf) = session_with(
            Some(vec![
                target(
                    "SLES",
                    "15.5",
                    &format!("{uri}/a"),
                    &[("foo", "1.2-3.1.x86_64")],
                ),
                target(
                    "SLES",
                    "16.0",
                    &format!("{uri}/b"),
                    &[("bar", "2-1.noarch")],
                ),
            ]),
            &[],
        );

        let args = matches(&ShowUpdateRepos, &["--check", "--json"]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();

        let out = stable(&buf.contents(), &server);
        assert_eq!(out.lines().count(), 1, "{out}");
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON line");
        insta::assert_snapshot!(serde_json::to_string_pretty(&parsed).unwrap());
    }

    #[tokio::test]
    async fn json_prints_a_failed_summary_when_no_client_can_be_built() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, buf) = session_with(
            Some(vec![target(
                "SLES",
                "15.5",
                "http://repo.invalid/a",
                LISTED,
            )]),
            &[],
        );
        session.config.ssl_verify = mtui_config::SslVerify::CaBundle(dir.path().join("none.pem"));
        let args = matches(&ShowUpdateRepos, &["--check", "--json"]);
        let err = ShowUpdateRepos.call(&mut session, &args).await.unwrap_err();
        assert!(
            err.to_string().starts_with("could not build HTTP client"),
            "{err}"
        );
        let out: serde_json::Value = serde_json::from_str(buf.contents().trim()).unwrap();
        assert_eq!(
            (&out["total"], &out["checked"], &out["ok"]),
            (
                &serde_json::json!(1),
                &serde_json::json!(0),
                &serde_json::json!(false)
            )
        );
    }

    #[tokio::test]
    async fn json_sanitises_reason_and_missing_names() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("foo", "1.2-3.1.x86_64")]).await;
        let (mut session, buf) = session_with(
            Some(vec![target(
                "SLES",
                "15.5",
                &format!("{}/a", server.uri()),
                &[("bad\u{1b}[31m\nname", "1-1.x86_64")],
            )]),
            &[],
        );
        let args = matches(&ShowUpdateRepos, &["--check", "--json"]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap_err();
        let out: serde_json::Value = serde_json::from_str(buf.contents().trim()).unwrap();
        let row = &out["repositories"][0];
        assert_eq!(row["missing"], serde_json::json!(["bad name"]));
        assert_eq!(
            row["reason"],
            "1 packages; not published at the listed version-release.arch: bad name"
        );
    }

    #[tokio::test]
    async fn check_runs_through_the_driver_with_no_host_connected() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("foo", "1.2-3.1.x86_64")]).await;
        let (mut session, buf) = session_with(
            Some(vec![target(
                "SLES",
                "15.5",
                &format!("{}/a", server.uri()),
                &[("foo", "1.2-3.1.x86_64")],
            )]),
            &[],
        );
        assert!(session.metadata().base().targets.is_empty());

        let args = matches(&ShowUpdateRepos, &["--check"]);
        ShowUpdateRepos.run(&mut session, &args).await.unwrap();
        assert!(
            buf.contents()
                .contains("    ok: 1 packages, all 1 listed binaries present"),
            "{}",
            buf.contents()
        );
    }

    /// Answers after `delay`, telling the test the moment it is asked.
    struct SignalOnHit {
        hit: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        delay: Duration,
    }

    impl wiremock::Respond for SignalOnHit {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            if let Some(tx) = self.hit.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ResponseTemplate::new(404).set_delay(self.delay)
        }
    }

    /// Four fast repositories (the first holding `first`) and a fifth that
    /// answers only after a long delay. With four checks at once the fifth
    /// starts only once the first has finished, so its request is the causal
    /// moment to cancel: the first row is done, the fifth is not.
    async fn session_cancelled_at_the_fifth(
        first: &[(&str, &str)],
    ) -> (Session, Buffer, MockServer) {
        let server = MockServer::start().await;
        serve_repo(&server, "r1", first).await;
        for dir in ["r2", "r3", "r4"] {
            serve_repo(&server, dir, &[("foo", "1.2-3.1.x86_64")]).await;
        }
        let (hit_tx, hit_rx) = tokio::sync::oneshot::channel();
        Mock::given(method("GET"))
            .and(path("/r5/repodata/repomd.xml"))
            .respond_with(SignalOnHit {
                hit: std::sync::Mutex::new(Some(hit_tx)),
                delay: Duration::from_secs(30),
            })
            .mount(&server)
            .await;
        let uri = server.uri();
        let foo: &[(&str, &str)] = &[("foo", "1.2-3.1.x86_64")];
        let (session, buf) = session_with(
            Some(vec![
                target("SL-Micro", "6.0", &format!("{uri}/r1"), foo),
                target("SL-Micro", "6.1", &format!("{uri}/r2"), foo),
                target("SLES", "15.5", &format!("{uri}/r3"), foo),
                target("SLES", "16.0", &format!("{uri}/r4"), foo),
                target("SLES", "16.1", &format!("{uri}/r5"), foo),
            ]),
            &[],
        );
        let cancel = session.cancel_token();
        tokio::spawn(async move {
            hit_rx.await.expect("the fifth repository must be asked");
            cancel.cancel();
        });
        (session, buf, server)
    }

    #[tokio::test]
    async fn a_cancel_reports_the_repositories_that_finished() {
        let (mut session, buf, server) =
            session_cancelled_at_the_fifth(&[("foo", "1.2-3.1.x86_64")]).await;

        let args = matches(&ShowUpdateRepos, &["--check"]);
        let err = ShowUpdateRepos.call(&mut session, &args).await.unwrap_err();
        let CommandError::Cancelled(detail) = &err else {
            panic!("expected a cancel, got {err:?}");
        };
        assert!(
            detail.starts_with("checked ") && detail.ends_with(" of 5 update repositories"),
            "{detail}"
        );
        let out = stable(&buf.contents(), &server);
        assert!(out.contains("http://repo.invalid/r1\n    ok: "), "{out}");
        assert!(!out.contains("/r5"), "{out}");
    }

    #[tokio::test]
    async fn a_failure_found_before_a_cancel_outranks_it() {
        let (mut session, buf, server) =
            session_cancelled_at_the_fifth(&[("foo", "1.2-2.1.x86_64")]).await;

        let args = matches(&ShowUpdateRepos, &["--check"]);
        let err = ShowUpdateRepos.call(&mut session, &args).await.unwrap_err();
        let CommandError::Other(message) = &err else {
            panic!("the missing repository must outrank the cancel, got {err:?}");
        };
        assert!(
            message.starts_with("1 of 5 update repositories are not ok; stopped after ")
                && message.ends_with(" of 5"),
            "{message}"
        );
        let out = stable(&buf.contents(), &server);
        assert!(
            out.contains("http://repo.invalid/r1\n    missing: "),
            "{out}"
        );
    }

    #[tokio::test]
    async fn the_overall_budget_turns_an_unfinished_repository_into_no_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let http = mtui_datasources::HttpClient::new(mtui_datasources::VerifyPolicy::Default(true))
            .unwrap();
        let got = check_all(
            &http,
            vec![(format!("{}/a", server.uri()), Listed::new())],
            Duration::from_millis(200),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            got,
            vec![Some(RepoCheck::NoAnswer(NoAnswer::OutOfTime(
                Duration::from_millis(200)
            )))]
        );
    }

    fn parsed(argv: &[&str]) -> ArgMatches {
        crate::engine::command_parser(&ShowUpdateRepos)
            .try_get_matches_from(argv)
            .unwrap()
    }

    #[tokio::test]
    async fn json_with_two_templates_loaded_never_fans_out() {
        let server = MockServer::start().await;
        serve_repo(&server, "a", &[("foo", "1.2-3.1.x86_64")]).await;
        let (mut session, buf) = session_with(
            Some(vec![target(
                "SLES",
                "15.5",
                &format!("{}/a", server.uri()),
                &[("foo", "1.2-3.1.x86_64")],
            )]),
            &[],
        );
        session.templates.add(fake_report(RRID_B, &[], "ok"));

        let err = ShowUpdateRepos
            .run(&mut session, &parsed(&["--check", "--json"]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CommandError::AmbiguousTemplate { .. }),
            "{err:?}"
        );
        assert_eq!(buf.contents(), "");

        let err = ShowUpdateRepos
            .run(
                &mut session,
                &parsed(&["--check", "--json", "--all-templates"]),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "--json reports one template; drop --all-templates and pick one with -T"
        );
        assert_eq!(buf.contents(), "");

        ShowUpdateRepos
            .run(&mut session, &parsed(&["--check", "--json", "-T", RRID]))
            .await
            .unwrap();
        let out = buf.contents();
        assert_eq!(out.lines().count(), 1, "{out}");
        let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(json["ok"], true);
    }

    #[test]
    fn json_needs_check() {
        let cmd =
            ShowUpdateRepos.configure(clap::Command::new("show_update_repos").no_binary_name(true));
        assert!(cmd.try_get_matches_from(["--json"]).is_err());
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ShowUpdateRepos.name(), "show_update_repos");
        assert_eq!(ShowUpdateRepos.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn empty_report_prints_none_line() {
        let (mut session, buf) = empty_session();
        let args = matches(&ShowUpdateRepos, &[]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("No update repos for this update."),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn lists_update_repos_sorted() {
        let (mut session, buf) = session_with_hosts(RRID, &["h1"], "ok");
        let base = session.metadata_mut().base_mut();
        base.update_repos.insert(
            mtui_types::SystemProduct::new("SLES", "15.5", "x86_64"),
            "http://repo/b".to_owned(),
        );
        base.update_repos.insert(
            mtui_types::SystemProduct::new("Basesystem", "15.5", "x86_64"),
            "http://repo/a".to_owned(),
        );
        let args = matches(&ShowUpdateRepos, &[]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        // Sorted by product name: Basesystem before SLES.
        let a = out.find("Basesystem").expect("Basesystem listed");
        let b = out.find("SLES").expect("SLES listed");
        assert!(a < b, "expected sorted order:\n{out}");
        assert!(out.contains("http://repo/a") && out.contains("http://repo/b"));
    }
}
