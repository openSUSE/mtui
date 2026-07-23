//! Resolution of openQA install-test job names to their log filenames, ported
//! from `mtui/data_sources/openqa_install.py`.
//!
//! Different install-test scenarios publish their zypper install log under
//! different filenames. The classic `qam-incidentinstall` (and the HA variant
//! `qam-incidentinstall-ha`) jobs publish `update_install-zypper.log` — the
//! value carried by the upstream `[openqa] install_logfile` config option. SLFO
//! jobs (`qam-incidentinstall-SLFO`) instead publish
//! `SLFO_update_install-zypper.log`.
//!
//! A single value cannot express this per-job-name divergence, so the URL
//! builders in the auto connector consult [`install_logfile_for`], which maps a
//! job name to its log filename by marker substring and falls back to a default
//! for the classic case.

/// Marker → install-log-filename overrides.
///
/// Matching is by substring so name variants (e.g. `qam-incidentinstall-SLFO-ha`)
/// resolve to the same SLFO log.
const INSTALL_LOGFILES: &[(&str, &str)] = &[("-SLFO", "SLFO_update_install-zypper.log")];

/// The classic install-log filename.
///
/// Upstream sources this from the `[openqa] install_logfile` config option.
/// That option is effectively obsolete (its value has not meaningfully changed
/// in practice), so it is pinned here as a constant rather than adding an
/// `[openqa]` config surface.
const DEFAULT_INSTALL_LOGFILE: &str = "update_install-zypper.log";

/// Return the install-log filename for an openQA install-test job.
///
/// Matching is by marker substring so name variants resolve to the same log;
/// unknown or empty names return [`DEFAULT_INSTALL_LOGFILE`].
///
/// Mirrors upstream `install_logfile_for(test_name, default)`, with the
/// `default` pinned to [`DEFAULT_INSTALL_LOGFILE`] (see the constant docs).
#[must_use]
pub(crate) fn install_logfile_for(test_name: &str) -> &'static str {
    for (marker, logfile) in INSTALL_LOGFILES {
        if test_name.contains(marker) {
            return logfile;
        }
    }
    DEFAULT_INSTALL_LOGFILE
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/test_openqa_install_map.py.

    #[test]
    fn classic_job_uses_default() {
        assert_eq!(
            install_logfile_for("qam-incidentinstall"),
            DEFAULT_INSTALL_LOGFILE
        );
        assert_eq!(
            install_logfile_for("qam-incidentinstall-ha"),
            DEFAULT_INSTALL_LOGFILE
        );
    }

    #[test]
    fn slfo_job_uses_slfo_logfile() {
        assert_eq!(
            install_logfile_for("qam-incidentinstall-SLFO"),
            "SLFO_update_install-zypper.log"
        );
    }

    #[test]
    fn slfo_marker_matches_variants() {
        assert_eq!(
            install_logfile_for("qam-incidentinstall-SLFO-ha"),
            "SLFO_update_install-zypper.log"
        );
    }

    #[test]
    fn unknown_and_empty_names_use_default() {
        assert_eq!(
            install_logfile_for("qam-somethingelse"),
            DEFAULT_INSTALL_LOGFILE
        );
        assert_eq!(install_logfile_for(""), DEFAULT_INSTALL_LOGFILE);
    }
}
