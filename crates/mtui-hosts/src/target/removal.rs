//! The removal guard: resolve a package transaction with a dry run first, and
//! refuse it on a host where the solver would remove a protected package the
//! command was not asked to remove.
//!
//! The probe is zypper's own dry run with XML output (`zypper -x … --dry-run`).
//! Its `<install-summary>` lists every package the transaction would remove
//! under `<to-remove>`, one `<solvable type="package" name="…">` each — the
//! layout `Summary::dumpAsXmlTo` writes unchanged from zypper 1.11 (SLE 12)
//! through 1.14. The captures under `tests/fixtures/zypper_dry_run/` pin it.

use std::collections::BTreeMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::operation::OperationGroup;

/// Packages no transaction may remove unless the command named them.
///
/// Matched exactly: their siblings (`kernel-default-devel`, `glibc-locale`,
/// `systemd-doc`) are routinely removed as dependants and are not needed to
/// boot, reach or repair the host. The list covers the kernel flavours, the
/// base system, the boot chain, the package and snapshot stack, and the
/// network, SSH and `sudo` access.
const PROTECTED: &[&str] = &[
    "NetworkManager",
    "aaa_base",
    "bash",
    "bash-sh",
    "coreutils",
    "dracut",
    "filesystem",
    "glibc",
    "kernel-64kb",
    "kernel-azure",
    "kernel-default",
    "kernel-default-base",
    "kernel-kvmsmall",
    "kernel-preempt",
    "kernel-rt",
    "kernel-zfcpdump",
    "kmod",
    "libzypp",
    "openssh",
    "openssh-common",
    "openssh-server",
    "pam",
    "rpm",
    "rpm-ndb",
    "shim",
    "snapper",
    "sudo",
    "systemd",
    "transactional-update",
    "tukit",
    "udev",
    "util-linux",
    "wicked",
    "zypper",
];

/// Matched as `grub2` or `grub2-<anything>`: the boot loader ships one package
/// per firmware and architecture (`grub2-x86_64-efi`, `grub2-arm64-efi`,
/// `grub2-i386-pc`, `grub2-powerpc-ieee1275`, …), any of which can be the one
/// the host boots with.
const PROTECTED_FAMILY: &[&str] = &["grub2"];

/// Whether removing `name` can leave a host unbootable or unreachable.
#[must_use]
pub fn is_protected(name: &str) -> bool {
    PROTECTED.contains(&name)
        || PROTECTED_FAMILY.iter().any(|family| {
            name.strip_prefix(family)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

/// The package names under `<to-remove>` in a `zypper -x` dry run's stdout.
///
/// A complete `<stream>` with no summary is an empty set: zypper prints the
/// summary before it commits anything, so without one nothing is committed.
///
/// # Errors
///
/// Returns why the output cannot be trusted: malformed XML, or a stream that
/// never reached `</stream>` (cut off, truncated, or not zypper's XML at all).
pub fn parse_removals(stdout: &str) -> Result<Vec<String>, String> {
    let mut reader = Reader::from_str(stdout);
    let mut in_remove = false;
    let mut closed = false;
    let mut removed = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == "to-remove" => in_remove = true,
            Ok(Event::End(e)) if e.name().as_ref() == "to-remove" => in_remove = false,
            Ok(Event::End(e)) if e.name().as_ref() == "stream" => closed = true,
            Ok(Event::Start(e) | Event::Empty(e))
                if in_remove && e.name().as_ref() == "solvable" =>
            {
                if attr(&e, "type").is_none_or(|t| t == "package") {
                    removed.push(attr(&e, "name").ok_or("a removed solvable has no name")?);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("malformed XML: {e}")),
            Ok(_) => {}
        }
    }
    if closed {
        Ok(removed)
    } else {
        Err("no complete zypper XML stream".to_owned())
    }
}

fn attr(e: &BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::default()).ok())
        .map(std::borrow::Cow::into_owned)
}

/// Whether a dry run's exit is one the install check accepts as success.
fn exit_accepted(exitcode: i32) -> bool {
    matches!(exitcode, 0 | 100..=103 | 106 | 107)
}

/// Whether a dry run exited as a successful one does and left a complete
/// stream; a failed one's stream carries zypper's reason.
#[must_use]
pub fn dry_run_succeeded(exitcode: i32, stdout: &str) -> bool {
    exit_accepted(exitcode) && parse_removals(stdout).is_ok()
}

/// Judges one host's dry run: its removal set, or why the transaction is
/// refused there.
///
/// Accepts the exit codes the install check accepts as success: `0` and
/// zypper's informational `100`-`103`, `106` and `107`.
///
/// # Errors
///
/// Returns the refusal reason when the dry run failed, its output cannot be
/// parsed, or it would remove a protected package that is not in `requested`.
pub fn judge_dry_run(
    exitcode: i32,
    stdout: &str,
    requested: &[String],
) -> Result<Vec<String>, String> {
    // `Target::run` records -1 when no exit code came back.
    if exitcode == -1 {
        return Err(
            "the dry run timed out or could not reach the host, so what the \
                    transaction would remove is unknown"
                .to_owned(),
        );
    }
    if !exit_accepted(exitcode) {
        return Err(format!(
            "the dry run failed (exit {exitcode}), so what the transaction would remove is unknown"
        ));
    }
    let removed = parse_removals(stdout).map_err(|e| {
        format!("the dry run's output could not be read ({e}), so what the transaction would remove is unknown")
    })?;
    let protected: Vec<&str> = removed
        .iter()
        .map(String::as_str)
        .filter(|name| is_protected(name) && !requested.iter().any(|r| r == name))
        .collect();
    if protected.is_empty() {
        return Ok(removed);
    }
    Err(format!(
        "the transaction would remove protected package(s) {} (all removals: {})",
        protected.join(", "),
        removed.join(", ")
    ))
}

/// What the removal guard found across a group.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DryRuns {
    /// Each probed host's verdict: the packages the transaction removes there,
    /// or why it is refused.
    pub verdicts: BTreeMap<String, Result<Vec<String>, String>>,
    /// The hosts with no dry run to judge, whose transaction runs unchecked.
    pub unchecked: Vec<String>,
}

/// Runs each host's dry run (`None`: the host has none the guard can read) and
/// judges it with [`judge_dry_run`].
///
/// Returns one verdict per probed host, and names every host without a probe
/// as unchecked.
pub async fn probe_removals(
    group: &mut dyn OperationGroup,
    probes: Vec<(String, Option<String>)>,
    requested: &[String],
) -> DryRuns {
    let mut commands = Vec::new();
    let mut unchecked = Vec::new();
    for (host, probe) in probes {
        match probe {
            Some(probe) => commands.push((host, probe)),
            None => {
                tracing::warn!(
                    host = %host,
                    "no dry run exists for this host; its removals are not checked"
                );
                unchecked.push(host);
            }
        }
    }
    if commands.is_empty() {
        return DryRuns {
            verdicts: BTreeMap::new(),
            unchecked,
        };
    }
    let hosts: Vec<String> = commands.iter().map(|(h, _)| h.clone()).collect();
    group.run(commands).await;
    let verdicts = hosts
        .into_iter()
        .map(|host| {
            let verdict = group.last_output(&host).map_or_else(
                || Err("no dry-run output was recorded".to_owned()),
                |o| judge_dry_run(o.exitcode, &o.stdout, requested),
            );
            match &verdict {
                Ok(removed) if !removed.is_empty() => {
                    tracing::info!(host = %host, removes = %removed.join(", "), "dry run");
                }
                Ok(_) => {}
                Err(reason) => tracing::error!(host = %host, %reason, "transaction refused"),
            }
            (host, verdict)
        })
        .collect();
    DryRuns {
        verdicts,
        unchecked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> &'static str {
        match name {
            "leap156_rm_libcurl4" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_rm_libcurl4.xml")
            }
            "leap156_rm_curl" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_rm_curl.xml")
            }
            "leap156_rm_libzypp" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_rm_libzypp.xml")
            }
            "leap156_in_busybox_less" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_in_busybox_less.xml")
            }
            "leap156_downgrade_less" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_downgrade_less.xml")
            }
            "leap156_in_bash_nothing_to_do" => include_str!(
                "../../tests/fixtures/zypper_dry_run/leap156_in_bash_nothing_to_do.xml"
            ),
            "leap156_rm_missing" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_rm_missing.xml")
            }
            "leap156_rm_glibc_problem" => {
                include_str!("../../tests/fixtures/zypper_dry_run/leap156_rm_glibc_problem.xml")
            }
            "tw_rm_libcurl4" => {
                include_str!("../../tests/fixtures/zypper_dry_run/tw_rm_libcurl4.xml")
            }
            other => panic!("no fixture {other}"),
        }
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn only_an_accepted_and_complete_dry_run_succeeded() {
        let complete = fixture("leap156_rm_curl");
        assert!(dry_run_succeeded(0, complete));
        assert!(dry_run_succeeded(106, complete));
        assert!(!dry_run_succeeded(104, complete));
        assert!(!dry_run_succeeded(-1, complete));
        assert!(!dry_run_succeeded(0, "<stream>"));
    }

    #[test]
    fn protected_matching_is_exact_except_for_the_grub2_family() {
        for name in [
            "zypper",
            "openssh-server",
            "kernel-default",
            "kernel-rt",
            "wicked",
            "grub2",
            "grub2-x86_64-efi",
            "kmod",
            "shim",
            "snapper",
            "sudo",
            "tukit",
        ] {
            assert!(is_protected(name), "{name}");
        }
        for name in [
            "zypper-log",
            "kernel-default-devel",
            "glibc-locale",
            "grub2x",
            "libcurl4",
        ] {
            assert!(!is_protected(name), "{name}");
        }
    }

    #[test]
    fn a_cascading_removal_lists_every_package_it_takes() {
        assert_eq!(
            parse_removals(fixture("leap156_rm_libcurl4")).unwrap(),
            strs(&["curl", "libcurl4", "libzypp", "zypper"])
        );
        assert_eq!(
            parse_removals(fixture("tw_rm_libcurl4")).unwrap(),
            strs(&[
                "curl",
                "libcurl-mini4",
                "libzypp",
                "zypp-excludedocs",
                "zypp-no-recommends",
                "zypper"
            ])
        );
    }

    #[test]
    fn only_the_to_remove_list_counts() {
        // `busybox-less` is in `<to-install>`; progress elements carry `name=` too.
        assert_eq!(
            parse_removals(fixture("leap156_in_busybox_less")).unwrap(),
            strs(&["less"])
        );
        assert!(
            parse_removals(fixture("leap156_downgrade_less"))
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_removals(fixture("leap156_in_bash_nothing_to_do"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn output_that_is_not_a_complete_stream_is_unreadable() {
        let full = fixture("leap156_rm_libcurl4");
        let cut = &full[..full.find("</to-remove>").unwrap()];
        assert!(parse_removals(cut).is_err());
        assert!(parse_removals("").is_err());
        assert!(parse_removals("ok").is_err());
        assert!(parse_removals("<stream><to-remove></stream>").is_err());
    }

    #[test]
    fn a_cascade_into_a_protected_package_is_refused() {
        let reason =
            judge_dry_run(0, fixture("leap156_rm_libcurl4"), &strs(&["libcurl4"])).unwrap_err();
        assert!(
            reason.contains("protected package(s) libzypp, zypper"),
            "{reason}"
        );
        assert!(
            reason.contains("curl, libcurl4, libzypp, zypper"),
            "{reason}"
        );
        let reason = judge_dry_run(0, fixture("tw_rm_libcurl4"), &strs(&["libcurl4"])).unwrap_err();
        assert!(
            reason.contains("protected package(s) libzypp, zypper"),
            "{reason}"
        );
    }

    #[test]
    fn unprotected_dependants_proceed() {
        assert_eq!(
            judge_dry_run(0, fixture("leap156_rm_curl"), &strs(&["curl"])),
            Ok(strs(&["curl"]))
        );
        assert_eq!(
            judge_dry_run(
                0,
                fixture("leap156_in_busybox_less"),
                &strs(&["busybox-less"])
            ),
            Ok(strs(&["less"]))
        );
    }

    #[test]
    fn a_protected_package_the_command_names_is_not_refused() {
        // Asked for libzypp alone: zypper still goes with it, unasked.
        let reason =
            judge_dry_run(0, fixture("leap156_rm_libzypp"), &strs(&["libzypp"])).unwrap_err();
        assert!(reason.contains("protected package(s) zypper ("), "{reason}");
        assert_eq!(
            judge_dry_run(
                0,
                fixture("leap156_rm_libzypp"),
                &strs(&["libzypp", "zypper"])
            ),
            Ok(strs(&["libzypp", "zypper"]))
        );
    }

    #[test]
    fn a_failed_or_unreadable_dry_run_fails_closed() {
        let reason = judge_dry_run(104, fixture("leap156_rm_missing"), &[]).unwrap_err();
        assert!(reason.contains("exit 104"), "{reason}");
        let reason = judge_dry_run(4, fixture("leap156_rm_glibc_problem"), &[]).unwrap_err();
        assert!(reason.contains("exit 4"), "{reason}");
        let reason = judge_dry_run(-1, "", &[]).unwrap_err();
        assert!(
            reason.starts_with("the dry run timed out or could not reach the host"),
            "{reason}"
        );
        let reason = judge_dry_run(0, "ok", &[]).unwrap_err();
        assert!(reason.contains("could not be read"), "{reason}");
        assert!(judge_dry_run(106, fixture("leap156_rm_curl"), &[]).is_ok());
    }
}
