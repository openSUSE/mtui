//! The `reload_products` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, host_op_budget, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::{PackageMeta, Reseed, Session};

/// Reloads and re-parses the products on the target reference hosts.
///
/// Re-runs the parse over each selected host's live connection via
/// [`Target::reload_system`](mtui_hosts::Target::reload_system). A host whose
/// products changed then has its tracked packages re-seeded from the active
/// report the way connect seeds them, and the output says what changed; an
/// unchanged host, or any host when the report carries no package metadata,
/// keeps them as they are. Best-effort: a host whose parse fails, or which
/// exhausts its own
/// [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT) budget, keeps its
/// previous system and the pass continues — but the command then fails naming
/// those hosts. `-t/--target`-only, hence [`Scope::Active`]:
/// the active template's host set, not once per loaded template.
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
        let package_meta = PackageMeta::of(session.metadata().base());
        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        let mut lines: Vec<String> = Vec::new();
        let mut failed: Vec<&str> = Vec::new();
        let budget = host_op_budget();
        let secs = budget.as_secs();
        // Per host, not one budget for the whole serial pass: a host that
        // exhausts its own is abandoned and the pass moves on.
        for name in &hosts {
            if let Some(t) = targets.get_mut(name) {
                let before = t.system().clone();
                match tokio::time::timeout(budget, t.reload_system()).await {
                    Err(_) => {
                        failed.push(name);
                        lines.push(format!("{name}: reload not completed within {secs}s"));
                        continue;
                    }
                    Ok(Err(e)) => {
                        failed.push(name);
                        lines.push(format!("{name}: reload failed ({e})"));
                        continue;
                    }
                    Ok(Ok(())) => {}
                }
                tracing::info!(host = %name, "reloaded products");
                let product = t.system().get_base().name.clone();
                lines.push(format!("{name}: reloaded products ({product})"));
                // With no package metadata there is nothing to seed from.
                if *t.system() == before || package_meta.is_empty() {
                    continue;
                }
                let reseed = package_meta.reseed(t);
                lines.extend(reseed_lines(name, &reseed, t.packages().is_empty()));
                // A cut-off query leaves only `current` stale, which every reader re-queries.
                if tokio::time::timeout(budget, t.query_versions())
                    .await
                    .is_err()
                {
                    lines.push(format!(
                        "{name}: tracked packages re-seeded, but reading their installed \
                         versions did not complete within {secs}s"
                    ));
                }
            }
        }

        for line in &lines {
            session.display.println(line);
        }
        if failed.is_empty() {
            return Ok(());
        }
        Err(CommandError::Other(format!(
            "reload_products failed on: {}",
            failed.join(", ")
        )))
    }
}

/// The output lines for a re-seeded host: what its tracked packages gained,
/// lost or now require, and the newly tracked ones `update` has yet to check.
fn reseed_lines(name: &str, reseed: &Reseed, tracks_none: bool) -> Vec<String> {
    let updated = reseed.updated.iter().map(|(p, old, new)| {
        let old = old
            .as_ref()
            .map_or_else(|| "unset".to_owned(), ToString::to_string);
        format!("~{p} (required {old} -> {new})")
    });
    let changes: Vec<String> = reseed
        .added
        .iter()
        .map(|p| format!("+{p}"))
        .chain(updated)
        .chain(reseed.dropped.iter().map(|p| format!("-{p}")))
        .collect();
    let mut line = format!("{name}: tracked packages: ");
    if changes.is_empty() {
        line.push_str("unchanged");
    } else {
        line.push_str(&changes.join(", "));
    }
    if !reseed.kept.is_empty() {
        line.push_str(&format!(
            " (kept for their recorded checks: {})",
            reseed.kept.join(", ")
        ));
    }
    if tracks_none {
        line.push_str("; no package of this update is tracked there now");
    }
    let mut lines = vec![line];
    match reseed.added.len() {
        0 => {}
        1 => lines.push(format!(
            "{name}: 1 newly tracked package was never checked; run `update` before `export`"
        )),
        n => lines.push(format!(
            "{name}: {n} newly tracked packages were never checked; run `update` before `export`"
        )),
    }
    lines
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
        // `-t`-only, so it stays active rather than fanning out per template.
        assert_eq!(ReloadProducts.scope(), Scope::Active);
    }

    #[tokio::test]
    async fn reloads_system_over_live_connection() {
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
        assert!(
            buf.contents().contains("h1: reloaded products (SLES)"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn a_failed_parse_fails_the_command_instead_of_echoing_the_old_product() {
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod", "sle-ha.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_open_error("/etc/products.d/sle-ha.prod");
        let target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "ok");
        session.targets_mut().add(target);

        let args = matches(&ReloadProducts, &["-t", "h1"]);
        let err = ReloadProducts.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("h1")),
            "{err}"
        );
        let out = buf.contents();
        assert!(!out.contains("h1: reloaded products"), "{out}");
        assert!(out.contains("h1: reload failed"), "{out}");
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

        // Sorted visit order puts the wedge first, so a per-host budget must
        // still reach `z-good`. `a-dead` answers no SFTP at all, which
        // `parse_system` opens first — scripting a product would only let it fail
        // fast instead of wedging.
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
        let err = testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                Duration::from_secs(5),
                ReloadProducts.call(&mut session, &args),
            )
            .await
            .expect("reload_products must return despite the wedged host")
            .expect_err("a host that never answered must not report success")
        })
        .await;

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("a-dead")),
            "{err}"
        );
        let out = buf.contents();
        assert!(out.contains("z-good: reloaded products (SLES)"), "{out}");
        assert!(!out.contains("a-dead: reloaded products"), "{out}");
        assert!(out.contains("a-dead: reload not completed"), "{out}");
    }

    // --- tracked packages follow the reloaded products ---

    use mtui_types::hostlog::CommandLog;
    use mtui_types::package::{Package, VersionCheck};
    use mtui_types::report_document::ReportDocument;
    use mtui_types::rpmver::RPMVersion;

    use crate::commands::testkit::{Buffer, session_with_targets};

    /// The version `MAINTENANCE_OBS` ships, the one installed before it, and a
    /// later build another target may ship.
    const SHIPPED: &str = "1.8.8-150600.3.9.1";
    const INSTALLED: &str = "1.8.8-150600.3.6.1";
    const NEWER: &str = "1.8.8-150600.3.12.1";

    fn product(name: &str) -> Vec<u8> {
        format!(
            "<product><name>{name}</name><baseversion>15</baseversion>\
             <patchlevel>6</patchlevel><arch>s390x</arch></product>"
        )
        .into_bytes()
    }

    /// An `rpm -q` answer reporting every package these tests track at `ver`.
    fn rpm_reading(ver: &str) -> CommandLog {
        let out = format!("smc-tools {ver}\nsmc-tools-completion {ver}\ncrmsh {ver}\n");
        CommandLog::new("", &out, "", 0, 0)
    }

    /// The installed-version query a host tracking `names` is sent.
    fn rpm_query(names: &[&str]) -> String {
        format!(
            "rpm -q --queryformat \"%{{Name}} %{{Version}}-%{{Release}}\n\" {}",
            mtui_types::shellquote::quote_args(names)
        )
    }

    /// A SLES 15-SP6 s390x host also carrying `addons` as `(file, product)`,
    /// on which every tracked package is installed at [`INSTALLED`].
    fn sles_host(host: &str, addons: &[(&str, &str)]) -> MockConnection {
        let listing = std::iter::once("SLES.prod").chain(addons.iter().map(|(file, _)| *file));
        addons.iter().fold(
            MockConnection::new(host)
                .with_listing("/etc/products.d", listing)
                .with_link("/etc/products.d/baseproduct", "SLES.prod")
                .with_file("/etc/products.d/SLES.prod", product("SLES"))
                .with_default(rpm_reading(INSTALLED)),
            |conn, (file, name)| conn.with_file(format!("/etc/products.d/{file}"), product(name)),
        )
    }

    /// Rewrites `file` on the host behind `conn` to hold `name`, as registering
    /// another product does; the clone shares `conn`'s files.
    fn change_product(conn: &MockConnection, file: &str, name: &str) {
        let _ = conn
            .clone()
            .with_file(format!("/etc/products.d/{file}"), product(name));
    }

    /// `MAINTENANCE_OBS`, whose only target is SLES-LTSS 15-SP6 s390x.
    fn ltss_document() -> ReportDocument {
        mtui_types::fixtures::MAINTENANCE_OBS.parse().unwrap()
    }

    /// [`ltss_document`] plus an HA target on the same base, shipping `crmsh`
    /// and a [`NEWER`] `smc-tools`.
    fn ltss_and_ha_document() -> ReportDocument {
        let mut doc = ltss_document();
        let mut ha = doc.install.targets[0].clone();
        ha.product = "SLE-Product-HA".to_owned();
        ha.version = "15-SP6".to_owned();
        ha.binaries = [
            ("crmsh".to_owned(), format!("{SHIPPED}.s390x")),
            ("smc-tools".to_owned(), format!("{NEWER}.s390x")),
        ]
        .into();
        doc.install.targets.push(ha);
        doc
    }

    /// A session whose active report carries `doc`'s package maps, with `host`
    /// connected (and seeded) over `conn`, and its display buffer.
    ///
    /// Each test names its own host: the in-process pool claim is shared by
    /// every session in the test binary.
    async fn connected(
        doc: &ReportDocument,
        host: &str,
        conn: MockConnection,
    ) -> (Session, Buffer) {
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", Vec::new());
        session.config.refhosts_resolvers = "path".to_owned();
        session.config.refhosts_path = "/nonexistent/refhosts.yml".into();
        use_document(&mut session, doc);
        session.inject_mock_connection(host, conn);
        let refused = session.add_named_hosts(vec![host.to_owned()]).await;
        assert!(refused.is_empty(), "{refused:?}");
        (session, buf)
    }

    /// The lines `buf` holds about `host`, past its `reloaded products` line.
    fn reseed_lines(buf: &Buffer, host: &str) -> Vec<String> {
        let prefix = format!("{host}: ");
        buf.contents()
            .lines()
            .filter(|l| l.starts_with(&prefix) && !l.contains("reloaded products"))
            .map(str::to_owned)
            .collect()
    }

    /// Points the active report's package maps at those ingested from `doc`.
    fn use_document(session: &mut Session, doc: &ReportDocument) {
        let mut ingested = mtui_testreport::TestReportBase::new(session.config.clone());
        mtui_testreport::apply_document(&mut ingested, doc);
        let base = session.metadata_mut().base_mut();
        base.packages = ingested.packages;
        base.host_packages = ingested.host_packages;
    }

    async fn reload(session: &mut Session, host: &str) {
        let args = matches(&ReloadProducts, &["-t", host]);
        ReloadProducts.call(session, &args).await.unwrap();
    }

    fn names(session: &Session, host: &str) -> Vec<String> {
        let t = session.targets().get(host).expect("attached");
        t.packages().iter().map(|p| p.name.clone()).collect()
    }

    fn package<'a>(session: &'a Session, host: &str, name: &str) -> &'a Package {
        let t = session.targets().get(host).expect("attached");
        t.packages().iter().find(|p| p.name == name).expect(name)
    }

    /// Records `name`'s checks as an update would: before at [`INSTALLED`], and
    /// after at [`NEWER`], which no version query in these tests answers.
    fn record_checks(session: &mut Session, host: &str, name: &str) {
        let t = session.targets_mut().get_mut(host).expect("attached");
        let pkg = t.packages_mut().iter_mut().find(|p| p.name == name);
        let pkg = pkg.expect(name);
        pkg.set_before(Some(INSTALLED)).unwrap();
        pkg.set_after(Some(NEWER)).unwrap();
    }

    fn assert_checks_survived(pkg: &Package) {
        assert_eq!(pkg.before_check(), &installed(INSTALLED), "{}", pkg.name);
        assert_eq!(pkg.after_check(), &installed(NEWER), "{}", pkg.name);
    }

    fn rpm(ver: &str) -> Option<RPMVersion> {
        Some(RPMVersion::parse(ver).unwrap())
    }

    fn installed(ver: &str) -> VersionCheck {
        VersionCheck::Installed(RPMVersion::parse(ver).unwrap())
    }

    /// A host connected with products no target names tracks nothing; once it
    /// registers the target's product, a reload seeds and queries it.
    #[tokio::test]
    async fn reload_seeds_a_host_that_gained_a_targets_product() {
        let host = "gained.invalid";
        let conn = sles_host(host, &[("addon.prod", "sle-module-basesystem")]);
        let (mut session, buf) = connected(&ltss_document(), host, conn.clone()).await;
        assert!(names(&session, host).is_empty());

        change_product(&conn, "addon.prod", "SLES-LTSS");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            [
                "gained.invalid: tracked packages: +smc-tools, +smc-tools-completion",
                "gained.invalid: 2 newly tracked packages were never checked; run `update` \
                 before `export`",
            ]
        );
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
        for name in ["smc-tools", "smc-tools-completion"] {
            let pkg = package(&session, host, name);
            assert_eq!(pkg.required(), Some(&RPMVersion::parse(SHIPPED).unwrap()));
            assert_eq!(pkg.current_check(), &installed(INSTALLED), "{name}");
        }
    }

    /// A host whose products did not change keeps exactly what it tracked,
    /// recorded versions included, and is not queried again.
    #[tokio::test]
    async fn reload_leaves_a_host_with_unchanged_products_alone() {
        let host = "unchanged.invalid";
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")]);
        let (mut session, buf) = connected(&ltss_document(), host, conn.clone()).await;
        record_checks(&mut session, host, "smc-tools");
        let issued = conn.commands().len();

        reload(&mut session, host).await;

        assert!(reseed_lines(&buf, host).is_empty(), "{}", buf.contents());
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
        assert_checks_survived(package(&session, host, "smc-tools"));
        assert_eq!(conn.commands().len(), issued, "{:?}", conn.commands());
    }

    /// A host keeping one target's product and gaining another keeps what it
    /// recorded for the first, under the higher required version where both
    /// ship a package, re-reads its installed versions, and starts tracking the
    /// second's packages. The output names the new package and says it was never
    /// checked, since the recorded checks alone would hide it from `export`.
    #[tokio::test]
    async fn reload_keeps_the_records_of_a_kept_product_and_adds_a_new_ones() {
        let host = "kept.invalid";
        // Connect queries the LTSS packages alone; once the update is
        // installed, every later query reads the shipped version.
        let conn = sles_host(
            host,
            &[
                ("SLES-LTSS.prod", "SLES-LTSS"),
                ("addon.prod", "sle-module-basesystem"),
            ],
        )
        .with_response(
            rpm_query(&["smc-tools", "smc-tools-completion"]),
            rpm_reading(INSTALLED),
        )
        .with_default(rpm_reading(SHIPPED));
        let (mut session, buf) = connected(&ltss_and_ha_document(), host, conn.clone()).await;
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
        assert_eq!(
            package(&session, host, "smc-tools").current_check(),
            &installed(INSTALLED)
        );
        record_checks(&mut session, host, "smc-tools");

        change_product(&conn, "addon.prod", "sle-ha");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            [
                "kept.invalid: tracked packages: +crmsh, ~smc-tools (required \
                 1.8.8-150600.3.9.1 -> 1.8.8-150600.3.12.1)",
                "kept.invalid: 1 newly tracked package was never checked; run `update` before \
                 `export`",
            ]
        );
        assert_eq!(
            names(&session, host),
            ["crmsh", "smc-tools", "smc-tools-completion"]
        );
        let kept = package(&session, host, "smc-tools");
        assert_eq!(kept.current_check(), &installed(SHIPPED));
        assert_checks_survived(kept);
        assert_eq!(kept.required(), rpm(NEWER).as_ref());
        let crmsh = package(&session, host, "crmsh");
        assert_eq!(crmsh.required(), rpm(SHIPPED).as_ref());
        assert_eq!(crmsh.current_check(), &installed(SHIPPED));
    }

    /// `doc` plus an HA target on the same base naming only `smc-tools`, at
    /// [`NEWER`].
    fn with_newer_ha_smc_tools(mut doc: ReportDocument) -> ReportDocument {
        let mut ha = doc.install.targets[0].clone();
        ha.product = "SLE-Product-HA".to_owned();
        ha.version = "15-SP6".to_owned();
        ha.binaries = [("smc-tools".to_owned(), format!("{NEWER}.s390x"))].into();
        doc.install.targets.push(ha);
        doc
    }

    /// A host gaining a product whose target names a package it already tracks
    /// at a higher version says the requirement rose, rather than `unchanged`.
    #[tokio::test]
    async fn reload_names_a_tracked_package_whose_required_version_rose() {
        let host = "raised.invalid";
        let conn = sles_host(
            host,
            &[
                ("SLES-LTSS.prod", "SLES-LTSS"),
                ("addon.prod", "sle-module-basesystem"),
            ],
        );
        let doc = with_newer_ha_smc_tools(ltss_document());
        let (mut session, buf) = connected(&doc, host, conn.clone()).await;

        change_product(&conn, "addon.prod", "sle-ha");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            [
                "raised.invalid: tracked packages: ~smc-tools (required 1.8.8-150600.3.9.1 -> \
              1.8.8-150600.3.12.1)"
            ]
        );
        assert_eq!(
            package(&session, host, "smc-tools").required(),
            rpm(NEWER).as_ref()
        );
    }

    /// A tracked package that had no required version names it as `unset`.
    #[tokio::test]
    async fn reload_names_a_required_version_a_tracked_package_lacked() {
        let host = "unset.invalid";
        let conn = sles_host(
            host,
            &[
                ("SLES-LTSS.prod", "SLES-LTSS"),
                ("addon.prod", "sle-module-basesystem"),
            ],
        );
        let mut doc = ltss_document();
        doc.install.targets[0]
            .binaries
            .insert("smc-tools".to_owned(), String::new());
        let doc = with_newer_ha_smc_tools(doc);
        let (mut session, buf) = connected(&doc, host, conn.clone()).await;
        assert_eq!(package(&session, host, "smc-tools").required(), None);

        change_product(&conn, "addon.prod", "sle-ha");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            ["unset.invalid: tracked packages: ~smc-tools (required unset -> 1.8.8-150600.3.12.1)"]
        );
    }

    /// A host that lost a target's product stops tracking that product's
    /// never-checked packages, but keeps one carrying recorded checks as it was.
    #[tokio::test]
    async fn reload_drops_only_the_unchecked_packages_of_a_product_the_host_lost() {
        let host = "lost.invalid";
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")]);
        let (mut session, buf) = connected(&ltss_document(), host, conn.clone()).await;
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
        record_checks(&mut session, host, "smc-tools");

        change_product(&conn, "addon.prod", "sle-module-basesystem");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            [
                "lost.invalid: tracked packages: -smc-tools-completion (kept for their recorded \
                 checks: smc-tools)"
            ]
        );
        assert_eq!(names(&session, host), ["smc-tools"]);
        let kept = package(&session, host, "smc-tools");
        assert_checks_survived(kept);
        assert_eq!(kept.required(), rpm(SHIPPED).as_ref());
    }

    /// Either recorded check alone keeps a lost product's package tracked.
    #[tokio::test]
    async fn reload_keeps_a_lost_products_package_with_either_check_recorded() {
        let host = "half-checked.invalid";
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")]);
        let (mut session, buf) = connected(&ltss_document(), host, conn.clone()).await;
        for pkg in session.targets_mut().get_mut(host).unwrap().packages_mut() {
            match pkg.name.as_str() {
                "smc-tools" => pkg.set_before(Some(INSTALLED)).unwrap(),
                _ => pkg.set_after(Some(NEWER)).unwrap(),
            }
        }

        change_product(&conn, "addon.prod", "sle-module-basesystem");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            [
                "half-checked.invalid: tracked packages: unchanged (kept for their recorded \
                 checks: smc-tools, smc-tools-completion)"
            ]
        );
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
    }

    /// A host that loses its last matching product tracks nothing, not the
    /// packages of another target whose version matches its base version
    /// (the HA target's `15-SP6`), and the output says so.
    #[tokio::test]
    async fn reload_tracks_nothing_once_the_last_matching_product_is_lost() {
        let host = "unmatched.invalid";
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")]);
        let (mut session, buf) = connected(&ltss_and_ha_document(), host, conn.clone()).await;
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);

        change_product(&conn, "addon.prod", "sle-module-basesystem");
        reload(&mut session, host).await;

        assert!(
            names(&session, host).is_empty(),
            "{:?}",
            names(&session, host)
        );
        assert_eq!(
            reseed_lines(&buf, host),
            [
                "unmatched.invalid: tracked packages: -smc-tools, -smc-tools-completion; no \
                 package of this update is tracked there now"
            ]
        );
    }

    /// With no package metadata loaded, a product change leaves the tracked
    /// packages alone: there is nothing to seed them from.
    #[tokio::test]
    async fn reload_without_package_metadata_leaves_the_tracked_packages_alone() {
        let host = "no-report.invalid";
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")]);
        let mut t = Target::with_connection(host, TargetState::Enabled, Box::new(conn));
        t.set_system(
            mtui_types::System::new(
                mtui_types::SystemProduct::new("SLES", "15-SP6", "s390x"),
                std::collections::BTreeSet::new(),
                false,
            ),
            false,
        );
        t.set_packages(vec![Package::new("bash")]);
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![t]);

        reload(&mut session, host).await;

        assert_eq!(names(&session, host), ["bash"]);
        assert!(reseed_lines(&buf, host).is_empty(), "{}", buf.contents());
    }

    /// A kept package whose new metadata version is empty, so the seed carries
    /// no required version, keeps the valid one it had.
    #[tokio::test]
    async fn reload_keeps_a_valid_required_version_over_an_empty_new_one() {
        let host = "unversioned.invalid";
        let conn = sles_host(
            host,
            &[
                ("SLES-LTSS.prod", "SLES-LTSS"),
                ("addon.prod", "sle-module-basesystem"),
            ],
        );
        let (mut session, buf) = connected(&ltss_document(), host, conn.clone()).await;
        let mut unversioned = ltss_document();
        unversioned.install.targets[0]
            .binaries
            .insert("smc-tools".to_owned(), String::new());
        use_document(&mut session, &unversioned);

        change_product(&conn, "addon.prod", "sle-ha");
        reload(&mut session, host).await;

        assert_eq!(
            reseed_lines(&buf, host),
            ["unversioned.invalid: tracked packages: unchanged"]
        );
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
        assert_eq!(
            package(&session, host, "smc-tools").required(),
            rpm(SHIPPED).as_ref()
        );
    }

    /// A version query that stalls after the products reloaded is reported as
    /// such, not as a failed reload: the products and the tracked list are in.
    #[tokio::test]
    async fn a_stalled_version_query_does_not_fail_the_reload() {
        let host = "stalled.invalid";
        // `parse_system` reads over SFTP only, so the delay stalls just the query.
        let conn = sles_host(host, &[("addon.prod", "SLES-LTSS")])
            .with_run_delay(std::time::Duration::from_secs(3600));
        let mut t = Target::with_connection(host, TargetState::Enabled, Box::new(conn));
        t.set_system(
            mtui_types::System::new(
                mtui_types::SystemProduct::new("SLES", "15-SP6", "s390x"),
                std::collections::BTreeSet::new(),
                false,
            ),
            false,
        );
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![t]);
        use_document(&mut session, &ltss_document());

        let args = matches(&ReloadProducts, &["-t", host]);
        crate::commands::testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                ReloadProducts.call(&mut session, &args),
            )
            .await
            .expect("reload_products must return despite the stalled query")
            .expect("the products did reload")
        })
        .await;

        let out = buf.contents();
        assert!(
            out.contains(&format!("{host}: reloaded products (SLES)")),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "{host}: tracked packages: +smc-tools, +smc-tools-completion\n"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "{host}: tracked packages re-seeded, but reading their installed versions did \
                 not complete within"
            )),
            "{out}"
        );
        assert_eq!(names(&session, host), ["smc-tools", "smc-tools-completion"]);
    }
}
