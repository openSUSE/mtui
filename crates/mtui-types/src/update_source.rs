//! Which update workflow mtui drives for a report.

/// Which update workflow mtui drives for a report, decided per-update from the
/// template metadata — NOT from the RRID's shape, which the SL-Micro 6.0/6.1
/// cutover makes undecidable (both workflows share the SLFO:1.1 id space).
///
/// **This is a selection, not an observation.** During the OBS-1.1 → git-1.1
/// transition an update may be served *both* ways at once, so the rule is a
/// precedence, not an exclusive inference: Gitea metadata present
/// (`gitea_commit_hash`) ⇒ `Git`, otherwise `Obs`. Being both is expected and
/// `Git` is the deliberate answer — do not add a branch for it. So `Obs` means
/// "drive the OBS workflow", not "only OBS-served": a dual-served update
/// resolves to `Git` and mtui leaves its OBS review request alone by design.
///
/// The variants mirror qem-dashboard's `incidents.type` column, whose value
/// qem-bot writes into the openQA BUILD string as `:{type}:{number}:{package}`
/// (qem-bot `types/submissions.py`). `Obs` is spelled `smelt` on that wire, and
/// `Default` is `Obs` to match qem-bot's `default_submission_type = "smelt"`.
///
/// Deliberately not named `Workflow`: `mtui_types::Workflow` already means the
/// test-execution workflow (auto/manual/kernel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateSource {
    /// Drive the build-service (IBS/OBS) workflow; `smelt` on the qem wire.
    #[default]
    Obs,
    /// Drive the Gitea workflow; `git` on the qem wire.
    Git,
}

impl UpdateSource {
    /// The qem-dashboard/qem-bot wire value for this source (`incidents.type`).
    #[must_use]
    pub const fn as_qem_type(self) -> &'static str {
        match self {
            Self::Obs => "smelt",
            Self::Git => "git",
        }
    }

    /// Parses a qem-dashboard `type` wire value. Unknown values and `""` fall
    /// back to [`Obs`](Self::Obs), matching qem-bot's own
    /// `default_submission_type = "smelt"`.
    #[must_use]
    pub fn from_qem_type(raw: &str) -> Self {
        match raw {
            "git" => Self::Git,
            _ => Self::Obs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_obs() {
        assert_eq!(UpdateSource::default(), UpdateSource::Obs);
    }

    #[test]
    fn round_trips_through_qem_wire_values() {
        for source in [UpdateSource::Obs, UpdateSource::Git] {
            assert_eq!(UpdateSource::from_qem_type(source.as_qem_type()), source);
        }
    }

    #[test]
    fn as_qem_type_matches_qem_bot_wire_strings() {
        assert_eq!(UpdateSource::Obs.as_qem_type(), "smelt");
        assert_eq!(UpdateSource::Git.as_qem_type(), "git");
    }

    #[test]
    fn from_qem_type_unknown_and_empty_fall_back_to_obs() {
        assert_eq!(UpdateSource::from_qem_type(""), UpdateSource::Obs);
        assert_eq!(UpdateSource::from_qem_type("bogus"), UpdateSource::Obs);
        assert_eq!(UpdateSource::from_qem_type("SMELT"), UpdateSource::Obs);
    }
}
