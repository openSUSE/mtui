//! The `show_diff` and `analyze_diff` commands.
//!
//! An OBS/osc `source.diff` is a concatenation of `header:` / `-----` delimited
//! sections (`changes files:`, `old:`, `new:`, `spec files:`, repeated `other
//! changes:`), *not* a plain unified diff, so it is parsed section by section.
//! `show_diff` pages the raw file; `analyze_diff` cross-checks patch definitions
//! against application and surfaces review-relevant references.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clap::ArgMatches;
use regex::Regex;
use std::sync::LazyLock;

use mtui_datasources::{Gitea, Osc};
use mtui_types::enums::RequestKind;

use super::apicall::{gitea_client, is_gitea_workflow, osc_client};
use super::support::{complete_with_templates, page_output};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

// Section framing.
static SECTION_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z0-9 ._-]*):\s*$").unwrap());
static SECTION_UNDERLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-{3,}\s*$").unwrap());
// `+Patch13:        some-fix.patch` style definitions in a spec file.
static PATCH_DEF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([+-])Patch(\d*):\s+(.*\.patch)\s*$").unwrap());
// `+%patch13 -p1` style application in a spec file.
static PATCH_APPLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[+-]%patch(\d*)\b").unwrap());
// `%autosetup` / `%autopatch` apply patches implicitly.
static AUTOMACRO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[+-]?\s*%auto(?:setup|patch)\b").unwrap());
// Source tarball / archive names in `old:` / `new:` sections.
static ARCHIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(?:tar(?:\.\w+)?|tgz|tbz2?|txz|zip|obscpio)\s*$").unwrap());
// Reviewer-relevant references from changelog text.
static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:CVE-\d{4}-\d+|b(?:sc|oo|nc)#\d+|jsc#\w+-\d+)\b").unwrap());
// `Version:` spec tags and `%define`/`%global …ver…` macros on +/- lines. The
// `\b` belongs to the macro alternative only: after `Version:` it would demand a
// word character next, matching `Version:1.2.3` but not `Version:` + space.
static VERSION_BUMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[+-]\s*(?:Version:|%(?:define|global)\s+\S*ver\w*\b)").unwrap()
});

/// A parsed `(header, body_lines)` section of a `source.diff`.
type Section = (String, Vec<String>);

/// Splits a `source.diff` into `(header, body)` sections, each starting with a
/// `header:` line immediately followed by a `-----` underline. Preamble before
/// the first header comes back under an empty header.
fn split_sections(text: &str) -> Vec<Section> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut sections: Vec<Section> = Vec::new();
    let mut header = String::new();
    let mut body: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(cap) = SECTION_HEADER.captures(line)
            && i + 1 < lines.len()
            && SECTION_UNDERLINE.is_match(lines[i + 1])
        {
            sections.push((std::mem::take(&mut header), std::mem::take(&mut body)));
            header = cap[1].to_owned();
            i += 2;
            continue;
        }
        body.push(line.to_owned());
        i += 1;
    }
    sections.push((header, body));
    sections
}

/// Every body whose header matches `name` (sections may repeat).
fn section_bodies<'a>(sections: &'a [Section], name: &str) -> Vec<&'a [String]> {
    sections
        .iter()
        .filter(|(h, _)| h == name)
        .map(|(_, b)| b.as_slice())
        .collect()
}

/// The flattened lines of every `spec files` body.
fn flatten<'a>(sections: &'a [Section], name: &str) -> Vec<&'a String> {
    section_bodies(sections, name)
        .into_iter()
        .flat_map(<[String]>::iter)
        .collect()
}

/// Parses `(added, removed)` patch definitions from spec lines. `added` is
/// `(number, filename)`; `removed` is filenames.
fn defined_patches(spec_lines: &[&String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for line in spec_lines {
        if let Some(cap) = PATCH_DEF.captures(line) {
            let sign = &cap[1];
            let number = cap[2].to_owned();
            let filename = cap[3].trim().to_owned();
            if sign == "+" {
                added.push((number, filename));
            } else {
                removed.push(filename);
            }
        }
    }
    (added, removed)
}

/// Inspects how patches are applied: returns
/// `(applied_numbers, uses_automacro)`. `uses_automacro` is true when
/// `%autosetup`/`%autopatch` is present or no `%patch` lines exist at all.
fn apply_info(spec_lines: &[&String]) -> (BTreeSet<String>, bool) {
    let mut applied = BTreeSet::new();
    let mut uses_automacro = false;
    let mut saw_patch_line = false;
    for line in spec_lines {
        if AUTOMACRO.is_match(line) {
            uses_automacro = true;
        }
        if let Some(cap) = PATCH_APPLY.captures(line) {
            saw_patch_line = true;
            applied.insert(cap[1].to_owned());
        }
    }
    if !saw_patch_line {
        uses_automacro = true;
    }
    (applied, uses_automacro)
}

/// Unique archive filenames from `old:`/`new:` bodies.
fn archives(bodies: &[&[String]]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for body in bodies {
        for line in body.iter() {
            let candidate = line.trim().trim_start_matches(['+', '-']).trim().to_owned();
            if !candidate.is_empty() && ARCHIVE.is_match(&candidate) && !names.contains(&candidate)
            {
                names.push(candidate);
            }
        }
    }
    names
}

/// Fetches a request's source diff from its review backend.
#[async_trait]
trait SourceDiffFetcher: Send + Sync {
    async fn fetch(&self) -> Result<String, CommandError>;
}

#[async_trait]
impl SourceDiffFetcher for Osc {
    async fn fetch(&self) -> Result<String, CommandError> {
        self.request_diff()
            .await
            .map_err(|e| CommandError::Other(format!("could not fetch the source diff: {e}")))
    }
}

#[async_trait]
impl SourceDiffFetcher for Gitea {
    async fn fetch(&self) -> Result<String, CommandError> {
        self.pr_diff()
            .await
            .map_err(|e| CommandError::Other(format!("could not fetch the source diff: {e}")))
    }
}

/// A fetcher that cannot fetch: the report has no diff source, or its client
/// could not be built. Deferred to fetch time so a cached `source.diff` never
/// needs a working client.
struct Unavailable(String);

#[async_trait]
impl SourceDiffFetcher for Unavailable {
    async fn fetch(&self) -> Result<String, CommandError> {
        Err(CommandError::Other(self.0.clone()))
    }
}

/// The fetcher for the loaded report's backend.
fn live_fetcher(session: &Session) -> Box<dyn SourceDiffFetcher> {
    let Some(rrid) = session.metadata().rrid().cloned() else {
        return Box::new(Unavailable("no report loaded".to_owned()));
    };
    if rrid.kind == RequestKind::Pi {
        return Box::new(Unavailable("no source diff for PI updates".to_owned()));
    }
    let fetcher: Result<Box<dyn SourceDiffFetcher>, CommandError> = if is_gitea_workflow(session) {
        gitea_client(session).map(|c| Box::new(c) as _)
    } else {
        osc_client(session, &rrid).map(|c| Box::new(c) as _)
    };
    fetcher.unwrap_or_else(|e| Box::new(Unavailable(e.to_string())))
}

/// Resolves, up front so the async part holds no `&Session`: where the loaded
/// report's `source.diff` lives, whether a missing file may be fetched (only a
/// report loaded from a document has no checkout), and the fetcher to use.
fn diff_source(
    session: &Session,
) -> Result<(PathBuf, bool, Box<dyn SourceDiffFetcher>), CommandError> {
    let wd = session
        .metadata()
        .base()
        .report_wd()
        .map_err(|e| CommandError::Other(format!("no report working directory: {e}")))?;
    Ok((
        wd.join("source.diff"),
        session.metadata().base().document.is_some(),
        live_fetcher(session),
    ))
}

/// The `source.diff` at `path`, or a clear error if unavailable.
///
/// A file there wins: it is the SVN checkout's copy, or an earlier fetch's
/// cache. Only when `may_fetch` does a missing file fall back to `fetch`, whose
/// result is then cached there.
async fn source_diff(
    path: &Path,
    may_fetch: bool,
    fetch: &dyn SourceDiffFetcher,
) -> Result<String, CommandError> {
    let read_err = match std::fs::read_to_string(path) {
        Ok(text) => return Ok(text),
        Err(e) => e,
    };
    if !may_fetch || read_err.kind() != std::io::ErrorKind::NotFound {
        return Err(CommandError::Other(format!(
            "{}: {read_err}",
            path.display()
        )));
    }
    let text = fetch.fetch().await?;
    mtui_config::atomic::write(text.as_bytes(), path)
        .map_err(|e| CommandError::Other(format!("{}: {e}", path.display())))?;
    Ok(text)
}

/// Shows the raw OBS source diff.
pub struct ShowDiff;

#[async_trait]
impl Command for ShowDiff {
    fn name(&self) -> &'static str {
        "show_diff"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Shows the raw OBS source diff.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let (path, may_fetch, fetcher) = diff_source(session)?;
        let text = source_diff(&path, may_fetch, fetcher.as_ref()).await?;
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        page_output(session, &lines).await;
        Ok(())
    }
}

/// Analyzes the OBS source diff to assist code review.
pub struct AnalyzeDiff;

#[async_trait]
impl Command for AnalyzeDiff {
    fn name(&self) -> &'static str {
        "analyze_diff"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Analyzes the OBS source diff to assist code review.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let (path, may_fetch, fetcher) = diff_source(session)?;
        let text = source_diff(&path, may_fetch, fetcher.as_ref()).await?;
        let sections = split_sections(&text);

        let spec_lines = flatten(&sections, "spec files");
        if spec_lines.is_empty() {
            session.display.println("No spec mentioned in source.diff");
            return Ok(());
        }

        let changes_text = flatten(&sections, "changes files")
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let (added_patches, removed_patches) = defined_patches(&spec_lines);
        let (applied, uses_automacro) = apply_info(&spec_lines);

        if !uses_automacro {
            let defined: BTreeSet<String> = added_patches.iter().map(|(n, _)| n.clone()).collect();
            if defined != applied {
                tracing::warn!(
                    defined = ?defined, applied = ?applied,
                    "patch numbers defined in the spec file do not match applied patches"
                );
            }
        }

        // Which added patches are referenced in the changelog?
        let mut mentioned: Vec<String> = Vec::new();
        for (_, filename) in &added_patches {
            if changes_text.contains(filename.as_str()) {
                mentioned.push(filename.clone());
            } else {
                tracing::warn!(patch = %filename, "patch isn't mentioned in any changelog");
            }
        }

        self.print_report(
            session,
            &sections,
            &added_patches,
            &removed_patches,
            &mentioned,
            uses_automacro,
            &changes_text,
        );
        Ok(())
    }
}

impl AnalyzeDiff {
    #[allow(clippy::too_many_arguments)]
    fn print_report(
        &self,
        session: &mut Session,
        sections: &[Section],
        added_patches: &[(String, String)],
        removed_patches: &[String],
        mentioned: &[String],
        uses_automacro: bool,
        changes_text: &str,
    ) {
        let d = &mut session.display;
        d.println("Source diff analysis:");
        d.println("");

        let old_archives = archives(&section_bodies(sections, "old"));
        let new_archives = archives(&section_bodies(sections, "new"));
        if !old_archives.is_empty() || !new_archives.is_empty() {
            d.println("  Source archives:");
            for name in &old_archives {
                d.println(&format!("    - {name}"));
            }
            for name in &new_archives {
                d.println(&format!("    + {name}"));
            }
            d.println("");
        }

        d.println("  Patches added in spec file:");
        if added_patches.is_empty() {
            d.println("    (none)");
        } else {
            for (i, (_, filename)) in added_patches.iter().enumerate() {
                let marker = if mentioned.contains(filename) {
                    ""
                } else {
                    "  (not in changelog)"
                };
                d.println(&format!("    {i} - {filename}{marker}"));
            }
        }
        d.println("");

        if !removed_patches.is_empty() {
            d.println("  Patches removed from spec file:");
            for (i, filename) in removed_patches.iter().enumerate() {
                d.println(&format!("    {i} - {filename}"));
            }
            d.println("");
        }

        if uses_automacro {
            d.println("  Patch application: %autosetup/%autopatch (apply check skipped)");
            d.println("");
        }

        let bumps: BTreeSet<String> = flatten(sections, "spec files")
            .iter()
            .filter(|l| VERSION_BUMP.is_match(l))
            .map(|l| l.trim().to_owned())
            .collect();
        if !bumps.is_empty() {
            d.println("  Version / macro changes:");
            for line in &bumps {
                d.println(&format!("    {line}"));
            }
            d.println("");
        }

        let references: BTreeSet<String> = REFERENCE
            .find_iter(changes_text)
            .map(|m| m.as_str().to_owned())
            .collect();
        if !references.is_empty() {
            d.println("  References in changelog:");
            d.println(&format!(
                "    {}",
                references.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn names_and_scopes() {
        assert_eq!(ShowDiff.name(), "show_diff");
        assert_eq!(ShowDiff.scope(), Scope::Fanout);
        assert_eq!(AnalyzeDiff.name(), "analyze_diff");
        assert_eq!(AnalyzeDiff.scope(), Scope::Fanout);
    }

    #[test]
    fn show_diff_complete_offers_templates_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = ShowDiff.complete(&session, "", "show_diff ");
        assert!(out.contains(&"-T".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[test]
    fn split_sections_frames_headers() {
        let text = "preamble\nspec files:\n-----\n+Patch1: fix.patch\nchanges files:\n-----\n- bsc#1 fixed\n";
        let s = split_sections(text);
        assert_eq!(section_bodies(&s, "spec files").len(), 1);
        assert_eq!(section_bodies(&s, "changes files").len(), 1);
    }

    #[test]
    fn defined_and_applied_patches() {
        let lines = [
            "+Patch1:  fix.patch".to_owned(),
            "-Patch2:  old.patch".to_owned(),
            "+%patch1 -p1".to_owned(),
        ];
        let refs: Vec<&String> = lines.iter().collect();
        let (added, removed) = defined_patches(&refs);
        assert_eq!(added, vec![("1".to_owned(), "fix.patch".to_owned())]);
        assert_eq!(removed, vec!["old.patch".to_owned()]);
        let (applied, automacro) = apply_info(&refs);
        assert!(applied.contains("1"));
        assert!(!automacro);
    }

    #[test]
    fn automacro_detected_and_no_patch_lines() {
        let lines = ["+%autosetup -p1".to_owned()];
        let refs: Vec<&String> = lines.iter().collect();
        assert!(apply_info(&refs).1);
        // No %patch and no automacro → still automacro, implicitly.
        let empty: Vec<&String> = Vec::new();
        assert!(apply_info(&empty).1);
    }

    #[test]
    fn archives_extracts_tarballs() {
        let body = vec![
            "  + foo-1.2.tar.gz".to_owned(),
            "  not-an-archive".to_owned(),
        ];
        let slices: Vec<&[String]> = vec![body.as_slice()];
        assert_eq!(archives(&slices), vec!["foo-1.2.tar.gz".to_owned()]);
    }

    #[test]
    fn version_bump_matches_plain_version_tag_lines() {
        // The RPM spec convention: `Version:`, whitespace, value.
        assert!(VERSION_BUMP.is_match("+Version:        1.2.3"));
        assert!(VERSION_BUMP.is_match("-Version:        1.2.2"));
        assert!(VERSION_BUMP.is_match("+Version:\t1.2.3"));
        assert!(VERSION_BUMP.is_match("+Version: 1.2.3"));
        // Value glued to the colon.
        assert!(VERSION_BUMP.is_match("+Version:1.2.3"));
        assert!(VERSION_BUMP.is_match("+version:        1.2.3"));
        assert!(VERSION_BUMP.is_match("+ Version:   1.2.3"));
    }

    #[test]
    fn version_bump_keeps_macro_branch_and_ignores_other_lines() {
        assert!(VERSION_BUMP.is_match("+%global libver 1.1"));
        assert!(VERSION_BUMP.is_match("-%define srcver 2.0"));
        assert!(VERSION_BUMP.is_match("+%define ver 1.0"));
        assert!(!VERSION_BUMP.is_match("+%global nothing here"));
        assert!(!VERSION_BUMP.is_match("+Release:        1"));
        assert!(!VERSION_BUMP.is_match("+BuildRequires:  pkgconfig"));
        assert!(!VERSION_BUMP.is_match("+Requires:       foo = %{version}"));
        assert!(!VERSION_BUMP.is_match("+#Version:       1.0"));
        // Unchanged context lines carry no +/- marker and are not bumps.
        assert!(!VERSION_BUMP.is_match(" Version:        1.2.3"));
    }

    #[tokio::test]
    async fn show_diff_no_report_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ShowDiff, &[]);
        let err = ShowDiff.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn analyze_diff_no_report_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&AnalyzeDiff, &[]);
        let err = AnalyzeDiff.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// Writes `source.diff` into the active report's working directory, so the
    /// diff commands read a real file.
    fn session_with_diff(
        diff: &str,
    ) -> (Session, crate::commands::testkit::Buffer, tempfile::TempDir) {
        use crate::commands::testkit::session_with_hosts;
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let dir = tempfile::tempdir().unwrap();
        // `report_wd()` is the parent of `path`.
        std::fs::write(dir.path().join("source.diff"), diff).unwrap();
        session.metadata_mut().base_mut().path = Some(dir.path().join("log"));
        (session, buf, dir)
    }

    #[tokio::test]
    async fn show_diff_pages_raw_file() {
        let (mut session, buf, _dir) = session_with_diff("line one\nline two\n");
        let args = matches(&ShowDiff, &[]);
        ShowDiff.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("line one"), "{out}");
        assert!(out.contains("line two"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn show_diff_interactive_quits_early_on_q() {
        use mtui_hosts::Prompter;

        // A tiny screen plus a `q`-answering prompter stops paging after the
        // first screen. `ACCTEST_*` is process-global, hence `#[serial(env)]`.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "3");
        }
        let (mut session, buf, _dir) = session_with_diff("first\nsecond\nthird\nfourth\nfifth\n");
        session.is_repl = true;
        session.set_prompter(Prompter::new(std::sync::Arc::new(|_t: String| {
            Box::pin(async move { Ok("q".to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        })));
        let args = matches(&ShowDiff, &[]);
        ShowDiff.call(&mut session, &args).await.unwrap();
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        let out = buf.contents();
        assert!(out.contains("first") && out.contains("second"), "{out}");
        assert!(!out.contains("fourth"), "should have quit early: {out}");
    }

    #[tokio::test]
    async fn analyze_diff_no_spec_reports_message() {
        let (mut session, buf, _dir) = session_with_diff("preamble only\n");
        let args = matches(&AnalyzeDiff, &[]);
        AnalyzeDiff.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("No spec mentioned in source.diff"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn analyze_diff_full_report() {
        let diff = "\
old:
-----
 foo-1.0.tar.gz
new:
-----
 foo-1.1.tar.gz
spec files:
-----
+Patch1:  fix-cve.patch
+%patch1 -p1
+%global libver 1.1
changes files:
-----
+ - fix-cve.patch resolves CVE-2024-1234 (bsc#1200000)
";
        let (mut session, buf, _dir) = session_with_diff(diff);
        let args = matches(&AnalyzeDiff, &[]);
        AnalyzeDiff.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Source diff analysis:"), "{out}");
        assert!(out.contains("Source archives:"), "{out}");
        assert!(out.contains("- foo-1.0.tar.gz"), "{out}");
        assert!(out.contains("+ foo-1.1.tar.gz"), "{out}");
        assert!(out.contains("Patches added in spec file:"), "{out}");
        assert!(out.contains("fix-cve.patch"), "{out}");
        assert!(out.contains("Version / macro changes:"), "{out}");
        assert!(out.contains("References in changelog:"), "{out}");
        assert!(out.contains("CVE-2024-1234"), "{out}");
        assert!(out.contains("bsc#1200000"), "{out}");
    }

    /// A `Version:`-only bump, with no `%define`/`%global`, must still produce
    /// the "Version / macro changes:" block.
    #[tokio::test]
    async fn analyze_diff_surfaces_plain_version_tag_bump() {
        let diff = "\
spec files:
-----
-Version:        1.2.2
+Version:        1.2.3
+Patch1:  fix.patch
+%autosetup -p1
";
        let (mut session, buf, _dir) = session_with_diff(diff);
        let args = matches(&AnalyzeDiff, &[]);
        AnalyzeDiff.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Version / macro changes:"), "{out}");
        assert!(out.contains("+Version:        1.2.3"), "{out}");
        assert!(out.contains("-Version:        1.2.2"), "{out}");
    }

    #[tokio::test]
    async fn analyze_diff_removed_and_automacro_and_unmentioned() {
        let diff = "\
spec files:
-----
+Patch2:  new.patch
-Patch3:  gone.patch
+%autosetup -p1
";
        let (mut session, buf, _dir) = session_with_diff(diff);
        let args = matches(&AnalyzeDiff, &[]);
        AnalyzeDiff.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Patches removed from spec file:"), "{out}");
        assert!(out.contains("gone.patch"), "{out}");
        assert!(out.contains("%autosetup/%autopatch"), "{out}");
        // new.patch is not in any changelog section.
        assert!(out.contains("(not in changelog)"), "{out}");
        // Over-match guard: nothing here is a version bump.
        assert!(!out.contains("Version / macro changes:"), "{out}");
    }

    mod fetch {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use mtui_types::UpdateSource;

        use super::*;

        /// A fetcher that counts its calls, standing in for the review backend.
        struct Counting {
            calls: AtomicUsize,
        }

        impl Counting {
            fn new() -> Self {
                Self {
                    calls: AtomicUsize::new(0),
                }
            }

            fn calls(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }
        }

        #[async_trait]
        impl SourceDiffFetcher for Counting {
            async fn fetch(&self) -> Result<String, CommandError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("spec files:\n-----\n+Patch1: fix.patch\n".to_owned())
            }
        }

        const DIFF: &str = "spec files:\n-----\n+Patch1: fix.patch\n";

        #[tokio::test]
        async fn a_missing_file_is_fetched_once_and_cached() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.diff");
            let stub = Counting::new();

            let first = source_diff(&path, true, &stub).await.unwrap();
            let second = source_diff(&path, true, &stub).await.unwrap();

            assert_eq!((first.as_str(), second.as_str()), (DIFF, DIFF));
            assert_eq!(stub.calls(), 1);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), DIFF);
        }

        #[tokio::test]
        async fn an_existing_file_is_read_without_fetching() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.diff");
            std::fs::write(&path, "from the checkout\n").unwrap();
            let stub = Counting::new();

            assert_eq!(
                source_diff(&path, true, &stub).await.unwrap(),
                "from the checkout\n"
            );
            assert_eq!(
                source_diff(&path, false, &stub).await.unwrap(),
                "from the checkout\n"
            );
            assert_eq!(stub.calls(), 0);
        }

        /// Without a document there is no backend to ask: today's error stays.
        #[tokio::test]
        async fn a_missing_file_without_a_document_is_an_error_and_never_fetched() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.diff");
            let stub = Counting::new();

            let err = source_diff(&path, false, &stub).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("source.diff")),
                "{err:?}"
            );
            assert_eq!(stub.calls(), 0);
            assert!(!path.exists());
        }

        fn document_session(id: &str) -> (Session, tempfile::TempDir) {
            let (mut session, _buf) = session_with_hosts(id, &["h1"], "ok");
            let dir = tempfile::tempdir().unwrap();
            let base = session.metadata_mut().base_mut();
            base.path = Some(dir.path().join("log"));
            base.document = Some(
                include_str!("../../../mtui-types/tests/fixtures/document/pi.json")
                    .parse()
                    .unwrap(),
            );
            (session, dir)
        }

        #[tokio::test]
        async fn a_product_increment_has_no_source_diff() {
            let (mut session, dir) = document_session("SUSE:PI:16.0:1");

            let err = ShowDiff
                .call(&mut session, &matches(&ShowDiff, &[]))
                .await
                .unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("no source diff for PI updates")),
                "{err:?}"
            );
            assert!(!dir.path().join("source.diff").exists());
        }

        #[tokio::test]
        async fn a_gitea_report_without_a_pr_url_names_the_missing_client() {
            let (mut session, _dir) = document_session("SUSE:SLFO:1.2:7787");
            session.metadata_mut().base_mut().update_source = UpdateSource::Git;

            let err = AnalyzeDiff
                .call(&mut session, &matches(&AnalyzeDiff, &[]))
                .await
                .unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("Gitea")),
                "{err:?}"
            );
        }

        /// A classic OBS document report goes to the OBS backend, which fails
        /// here on the missing oscrc without touching the network.
        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes
        // the mutation of the process-global `$OSC_CONFIG` exclusive.
        #[allow(unsafe_code)]
        async fn an_obs_report_asks_the_obs_backend() {
            let (mut session, dir) = document_session("SUSE:Maintenance:1:1");

            // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
            unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
            let res = ShowDiff.call(&mut session, &matches(&ShowDiff, &[])).await;
            // SAFETY: still inside that critical section.
            unsafe { std::env::remove_var("OSC_CONFIG") };

            let err = res.unwrap_err();
            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("could not fetch the source diff")),
                "{err:?}"
            );
            assert!(!dir.path().join("source.diff").exists());
        }
    }
}
