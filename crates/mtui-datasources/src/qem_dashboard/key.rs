//! The QEM Dashboard's incident identity.
//!
//! The dashboard identifies an incident by `(number, project, type)`; its
//! per-incident endpoints answer 400 unless the request carries `project` and
//! `type` next to the number in the path.

use mtui_types::UpdateSource;
use reqwest::Url;

/// A fully qualified dashboard incident: the number plus the `project` and
/// `type` the dashboard needs to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentKey {
    /// The dashboard incident number (a maintenance id or a Gitea PR number).
    pub number: String,
    /// The dashboard `project`: `SUSE:Maintenance:<number>` for a classic
    /// update, the Gitea base repo's `owner/repo` for a git one.
    pub project: String,
    /// The dashboard `type`, kept as the report's [`UpdateSource`].
    pub source: UpdateSource,
}

impl IncidentKey {
    /// Derive the key locally, without asking the dashboard.
    ///
    /// A classic update's project follows from the number alone. A git update's
    /// project is the `owner/repo` preceding `/pulls/` in `pr_url`, in either
    /// the web (`https://host/owner/repo/pulls/N`) or the API
    /// (`https://host/api/v1/repos/owner/repo/pulls/N`) form. `None` when
    /// `pr_url` is missing or has no such shape.
    #[must_use]
    pub fn derive(number: &str, source: UpdateSource, pr_url: Option<&str>) -> Option<Self> {
        let project = match source {
            UpdateSource::Obs => format!("SUSE:Maintenance:{number}"),
            UpdateSource::Git => repo_of_pull_request(pr_url?)?,
        };
        Some(Self {
            number: number.to_owned(),
            project,
            source,
        })
    }

    /// The query string the per-incident endpoints require.
    #[must_use]
    pub fn query(&self) -> String {
        format!(
            "project={}&type={}",
            urlencoding::encode(&self.project),
            self.source.as_qem_type()
        )
    }
}

fn repo_of_pull_request(pr_url: &str) -> Option<String> {
    let url = Url::parse(pr_url).ok()?;
    let segments: Vec<&str> = url.path_segments()?.collect();
    let pulls = segments.iter().rposition(|s| *s == "pulls")?;
    let [owner, repo] = segments.get(pulls.checked_sub(2)?..pulls)? else {
        return None;
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smelt_project_is_built_from_the_number() {
        let key = IncidentKey::derive("30071", UpdateSource::Obs, None).unwrap();
        assert_eq!(key.number, "30071");
        assert_eq!(key.project, "SUSE:Maintenance:30071");
        assert_eq!(key.source, UpdateSource::Obs);
    }

    #[test]
    fn smelt_ignores_a_pr_url() {
        let key = IncidentKey::derive(
            "30071",
            UpdateSource::Obs,
            Some("https://src.suse.de/products/SLFO/pulls/8938"),
        )
        .unwrap();
        assert_eq!(key.project, "SUSE:Maintenance:30071");
    }

    #[test]
    fn git_project_comes_from_the_web_url() {
        let key = IncidentKey::derive(
            "8938",
            UpdateSource::Git,
            Some("https://src.suse.de/products/SLFO/pulls/8938"),
        )
        .unwrap();
        assert_eq!(key.project, "products/SLFO");
        assert_eq!(key.source, UpdateSource::Git);
    }

    #[test]
    fn git_project_comes_from_the_api_url() {
        let key = IncidentKey::derive(
            "8938",
            UpdateSource::Git,
            Some("https://src.suse.de/api/v1/repos/products/SLFO/pulls/8938"),
        )
        .unwrap();
        assert_eq!(key.project, "products/SLFO");
    }

    #[test]
    fn git_without_a_usable_url_is_unresolved() {
        for pr_url in [
            None,
            Some(""),
            Some("8938"),
            Some("not a url"),
            Some("https://src.suse.de/pulls/8938"),
            Some("https://src.suse.de/SLFO/pulls/8938"),
            Some("https://src.suse.de/products/SLFO/issues/8938"),
            Some("https://src.suse.de//SLFO/pulls/8938"),
        ] {
            assert_eq!(
                IncidentKey::derive("8938", UpdateSource::Git, pr_url),
                None,
                "{pr_url:?}"
            );
        }
    }

    #[test]
    fn query_percent_encodes_the_project() {
        let git = IncidentKey::derive(
            "8938",
            UpdateSource::Git,
            Some("https://src.suse.de/products/SLFO/pulls/8938"),
        )
        .unwrap();
        assert_eq!(git.query(), "project=products%2FSLFO&type=git");

        let smelt = IncidentKey::derive("30071", UpdateSource::Obs, None).unwrap();
        assert_eq!(
            smelt.query(),
            "project=SUSE%3AMaintenance%3A30071&type=smelt"
        );
    }
}
