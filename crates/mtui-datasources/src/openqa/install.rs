//! Resolution of openQA install-test job names to their log filenames.
//!
//! Different install-test scenarios publish their zypper install log under
//! different filenames. The classic `qam-incidentinstall` (and the HA variant
//! `qam-incidentinstall-ha`) jobs publish `update_install-zypper.log`. SLFO
//! jobs (`qam-incidentinstall-SLFO`) instead publish
//! `SLFO_update_install-zypper.log`.
//!
//! The auto connector's URL builders therefore consult [`install_logfile_for`],
//! which maps a job name to its log filename by marker substring and falls back
//! to the classic default.

/// Marker → install-log-filename overrides.
///
/// Matching is by substring so name variants (e.g. `qam-incidentinstall-SLFO-ha`)
/// resolve to the same SLFO log.
const INSTALL_LOGFILES: &[(&str, &str)] = &[("-SLFO", "SLFO_update_install-zypper.log")];

/// The classic install-log filename.
///
/// Unchanged in practice, so pinned here rather than given an `[openqa]` config
/// surface.
const DEFAULT_INSTALL_LOGFILE: &str = "update_install-zypper.log";

/// Return the install-log filename for an openQA install-test job.
///
/// Matching is by marker substring so name variants resolve to the same log;
/// unknown or empty names return [`DEFAULT_INSTALL_LOGFILE`].
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
