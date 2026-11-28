//! Where an update's review stands, read without loading it: who holds the QA
//! review, the state of the group's review, and whether the template matches
//! the update. Typed data for `review_state` and any other reader; nothing
//! here assigns, locks, regenerates or loads.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use mtui_config::Config;
use mtui_datasources::obs::gate::{GroupOwner, GroupVerdict, RequestGate};
use mtui_datasources::{
    DocumentFetch, Gitea, GiteaError, HttpClient, Osc, PullReviewGate, PullState, PullVerdict,
    TeReGen, TemplateStatus, TeregenV2, TeregenV2Error, sanitize_url,
};
use mtui_types::report_document::{DocWorkflow, ReportDocument};
use mtui_types::{RequestKind, RequestReviewID};
use serde::Serialize;

use crate::commands::support::one_line;

/// Cap on a reason or value taken from a server.
const TEXT_LIMIT: usize = 300;

/// A value that is known, unknown (it could not be read), absent (nothing to
/// show) or does not apply to this kind of update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum Field<T> {
    /// Read.
    Known(T),
    /// Could not be read.
    Unknown,
    /// Read, and there is nothing.
    None,
    /// Has no meaning for this update.
    NotApplicable,
}

impl<T: fmt::Display> fmt::Display for Field<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(v) => v.fmt(f),
            Self::Unknown => f.write_str("?"),
            Self::None => f.write_str("-"),
            Self::NotApplicable => f.write_str("n/a"),
        }
    }
}

impl<T> Field<T> {
    fn known_or_unknown(value: Option<T>) -> Self {
        value.map_or(Self::Unknown, Self::Known)
    }
}

/// The review backend an update is served by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Workflow {
    /// A Gitea pull request.
    Gitea,
    /// An OBS/IBS request.
    Obs,
}

/// Who holds one group's review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Owner {
    /// Nobody.
    None,
    /// This login.
    Known {
        /// The holder's login.
        login: String,
    },
    /// Someone holds it, but the source names them with a blank login.
    Unknown,
    /// The open reviews and the review history disagree.
    Disputed {
        /// The holder by the open user review, if any.
        hold: Option<String>,
        /// The users the review history leaves assigned.
        history: Vec<String>,
    },
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("-"),
            Self::Known { login } => f.write_str(login),
            Self::Unknown | Self::Disputed { .. } => f.write_str("?"),
        }
    }
}

/// The state of a group's review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    /// Gitea `REQUEST_REVIEW`: the group's verdict is owed.
    Owed,
    /// Gitea `APPROVED`.
    Approved,
    /// Gitea `REQUEST_CHANGES`.
    ChangesRequested,
    /// OBS: the group review is open and nobody took it.
    New,
    /// OBS: an assign accepted the group review and its user review is open.
    Assigned,
    /// OBS: the group review is accepted and no user review it opened is open.
    Accepted,
}

impl fmt::Display for GateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Owed => "owed",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes-requested",
            Self::New => "new",
            Self::Assigned => "new (assigned)",
            Self::Accepted => "accepted",
        })
    }
}

/// A group's review gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Gate {
    /// The group has no review.
    None,
    /// Read.
    Known {
        /// The review's state.
        state: GateState,
        /// A push followed the verdict.
        stale: bool,
        /// The pull request was closed with the verdict still owed.
        closed: bool,
    },
    /// The reviews could not be read as a gate.
    Unknown,
}

impl fmt::Display for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("-"),
            Self::Unknown => f.write_str("?"),
            Self::Known { closed: true, .. } => f.write_str("closed"),
            Self::Known {
                state, stale: true, ..
            } => write!(f, "{state}, stale"),
            Self::Known { state, .. } => state.fmt(f),
        }
    }
}

/// One group's review of an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupReview {
    /// The review group.
    pub group: String,
    /// An automation group (`qam-auto`, `qam-openqa`) rather than testers.
    pub automated: bool,
    /// Who holds it.
    pub owner: Owner,
    /// Where it stands.
    pub gate: Gate,
    /// When it got there.
    pub since: Field<String>,
}

/// The template's state against the update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Template {
    /// It exists and, for a pull request, was generated from its head; for an
    /// OBS request only its existence is checked.
    Current,
    /// It exists but was generated from another commit, or TeReGen flags it.
    Stale,
    /// There is none.
    Missing {
        /// The first line of the generation job's error.
        error: Option<String>,
    },
    /// A generation job is running.
    Generating,
    /// Could not be read.
    Unknown,
    /// A pull request URL names no RRID, so there is no template to ask for.
    NotApplicable,
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current => f.write_str("current"),
            Self::Stale => f.write_str("stale"),
            Self::Missing { error: None } => f.write_str("missing"),
            Self::Missing { error: Some(e) } => write!(f, "missing: {e}"),
            Self::Generating => f.write_str("generating"),
            Self::Unknown => f.write_str("?"),
            Self::NotApplicable => f.write_str("n/a"),
        }
    }
}

/// The source a failure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The Gitea pull request.
    Gitea,
    /// The OBS request.
    Obs,
    /// TeReGen's template status.
    Teregen,
    /// The published report document.
    Document,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gitea => "gitea",
            Self::Obs => "obs",
            Self::Teregen => "teregen",
            Self::Document => "document",
        })
    }
}

/// A source that could not be read for one update, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// The update, as named.
    pub update: String,
    /// Which source.
    pub source: Source,
    /// Why, as one sanitised line.
    pub reason: String,
}

/// Where one update's review stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateReview {
    /// The update as the caller named it, credentials stripped.
    pub update: String,
    /// The backend serving it.
    pub workflow: Field<Workflow>,
    /// OBS: the request's own state; Gitea: not applicable.
    pub state: Field<String>,
    /// When the request entered `state`.
    pub state_since: Field<String>,
    /// One entry per review group.
    pub reviews: Field<Vec<GroupReview>>,
    /// When the pull request or request was created.
    pub created: Field<String>,
    /// The pull request head.
    pub head: Field<String>,
    /// The pull request's base branch, as Gitea names it; mtui does not judge
    /// it against the RRID.
    pub base: Field<String>,
    /// The template.
    pub template: Template,
    /// The sources that could not be read (not serialised with the row: a
    /// listing carries them once, beside its rows).
    #[serde(skip)]
    pub failures: Vec<Failure>,
    /// Things read that need a caveat (not serialised, as `failures`).
    #[serde(skip)]
    pub notes: Vec<String>,
}

impl UpdateReview {
    fn new(update: String) -> Self {
        Self {
            update,
            workflow: Field::Unknown,
            state: Field::Unknown,
            state_since: Field::Unknown,
            reviews: Field::Unknown,
            created: Field::Unknown,
            head: Field::Unknown,
            base: Field::Unknown,
            template: Template::Unknown,
            failures: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn fail(&mut self, source: Source, reason: &str) {
        self.failures.push(Failure {
            update: self.update.clone(),
            source,
            reason: one_line(reason, Some(TEXT_LIMIT)),
        });
    }

    /// Whether the request is still in review (always, for a pull request).
    #[must_use]
    pub fn in_review(&self) -> bool {
        match &self.state {
            Field::Known(state) => matches!(state.as_str(), "new" | "review"),
            _ => true,
        }
    }
}

/// An update as a caller named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// By RRID.
    Rrid(RequestReviewID),
    /// By pull request, as its API URL.
    Pull {
        /// `<origin>/api/v1/repos/<owner>/<repo>/pulls/<n>`.
        api: String,
    },
}

/// Parse an RRID or a pull request URL.
///
/// # Errors
///
/// A one-line reason naming the argument with any credentials stripped.
pub fn parse_target(raw: &str) -> Result<Target, String> {
    if let Ok(rrid) = RequestReviewID::parse(raw) {
        return Ok(Target::Rrid(rrid));
    }
    pull_api(raw)
        .map(|(api, _)| Target::Pull { api })
        .ok_or_else(|| {
            format!(
                "{}: neither an RRID nor a pull request URL (https://<host>/<owner>/<repo>/pulls/<n>)",
                one_line(&sanitize_url(raw), Some(TEXT_LIMIT))
            )
        })
}

/// A pull request's API URL and number, from its web or its API URL. Userinfo
/// is dropped, so a credential in the argument never travels.
fn pull_api(url: &str) -> Option<(String, u64)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let segments: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
    let pulls = segments.iter().rposition(|s| *s == "pulls")?;
    if segments.len() != pulls + 2 {
        return None;
    }
    let number: u64 = segments[pulls + 1].parse().ok()?;
    let [owner, repo] = segments.get(pulls.checked_sub(2)?..pulls)? else {
        return None;
    };
    let host = parsed.host_str()?;
    let origin = match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    };
    Some((
        format!("{origin}/api/v1/repos/{owner}/{repo}/pulls/{number}"),
        number,
    ))
}

/// What a read needs, owned so it can run off a `!Sync` session.
#[derive(Clone)]
pub struct ReadContext {
    /// The configuration naming every source.
    pub config: Config,
    /// The shared transport.
    pub http: HttpClient,
    /// The wall-clock budget of one source's read for one update.
    pub budget: Duration,
}

/// `fut`, bounded by `budget`; its error or the timeout as one line.
async fn bounded<T, E: fmt::Display>(
    budget: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(budget, fut).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err(format!("no answer within {budget:?}")),
    }
}

/// The published document, as far as a review read needs it.
enum DocRead {
    Doc(Box<ReportDocument>),
    /// TeReGen holds a document but flags it stale.
    Stale,
    Failed(String),
}

async fn read_document(ctx: &ReadContext, rrid: &str) -> DocRead {
    let v2 = TeregenV2::with_client(ctx.http.clone(), &ctx.config.teregen_api_v2);
    match tokio::time::timeout(ctx.budget, v2.fetch_document(rrid, None)).await {
        Ok(Ok(DocumentFetch::Fresh { document, .. })) => DocRead::Doc(document),
        Ok(Ok(DocumentFetch::NotModified)) => {
            DocRead::Failed("TeReGen answered 304 to an unconditional read".to_owned())
        }
        Ok(Err(TeregenV2Error::Stale)) => DocRead::Stale,
        Ok(Err(TeregenV2Error::NotFound)) => {
            DocRead::Failed("TeReGen has no published document for it".to_owned())
        }
        Ok(Err(e)) => DocRead::Failed(e.to_string()),
        Err(_) => DocRead::Failed(format!("no answer within {:?}", ctx.budget)),
    }
}

async fn read_status(ctx: &ReadContext, rrid: &str) -> Result<TemplateStatus, String> {
    let teregen = TeReGen::with_client(ctx.http.clone(), &ctx.config.teregen_api);
    bounded(ctx.budget, teregen.template_status(rrid)).await
}

async fn read_pull(ctx: &ReadContext, api: &str) -> Result<PullState, String> {
    if ctx.config.gitea_token.is_empty() {
        return Err(GiteaError::MissingToken.to_string());
    }
    let gitea = Gitea::with_client(
        ctx.http.clone(),
        ctx.config.gitea_token.clone(),
        ctx.config.session_user.clone(),
        api,
        &ctx.config.gitea_url,
        None,
    )
    .map_err(|e| e.to_string())?;
    bounded(ctx.budget, gitea.pull_state()).await
}

async fn read_obs(ctx: &ReadContext, rrid: &RequestReviewID) -> Result<RequestGate, String> {
    let osc = Osc::new(ctx.config.clone(), rrid.clone(), ctx.http.clone());
    bounded(ctx.budget, osc.request_gate()).await
}

/// How the template is checked against the update once it exists.
enum Commit {
    /// No head to compare with (an OBS request).
    NotNeeded,
    /// TeReGen flags its document stale.
    ServerStale,
    /// The document's origin commit against the pull request head.
    Compare {
        commit: Option<String>,
        head: Option<String>,
    },
    /// Could not be checked; the reason is already listed.
    Unknown,
}

/// Read where one update's review stands. `update` is how it is shown.
pub async fn read_update(ctx: &ReadContext, update: String, target: Target) -> UpdateReview {
    let mut out = UpdateReview::new(update);
    match target {
        Target::Pull { api } => {
            out.workflow = Field::Known(Workflow::Gitea);
            fill_pull(&mut out, read_pull(ctx, &api).await);
            out.template = Template::NotApplicable;
        }
        Target::Rrid(rrid) => {
            let id = rrid.to_string();
            if rrid.kind == RequestKind::Slfo {
                let (status, doc) = tokio::join!(read_status(ctx, &id), read_document(ctx, &id));
                let commit = match doc {
                    DocRead::Doc(doc) if matches!(doc.workflow, DocWorkflow::Gitea) => {
                        out.workflow = Field::Known(Workflow::Gitea);
                        match pull_of(&doc, &rrid) {
                            Ok(api) => {
                                let pull = read_pull(ctx, &api).await;
                                let head = pull.as_ref().ok().map(|p| p.head.clone());
                                fill_pull(&mut out, pull);
                                Commit::Compare {
                                    commit: doc.update.origin.commit.clone(),
                                    head,
                                }
                            }
                            Err(reason) => {
                                out.fail(Source::Document, &reason);
                                Commit::Unknown
                            }
                        }
                    }
                    DocRead::Doc(_) => {
                        out.workflow = Field::Known(Workflow::Obs);
                        fill_obs(&mut out, &rrid, read_obs(ctx, &rrid).await);
                        Commit::NotNeeded
                    }
                    DocRead::Stale => {
                        out.fail(
                            Source::Document,
                            "TeReGen flags the published document stale, so it names no pull \
                             request to read; pass the pull request URL",
                        );
                        Commit::ServerStale
                    }
                    DocRead::Failed(reason) => {
                        out.fail(
                            Source::Document,
                            &format!(
                                "{reason}; without the published document there is no pull \
                                 request to read: pass its URL"
                            ),
                        );
                        Commit::Unknown
                    }
                };
                out.template = template_of(&mut out, status, commit);
            } else {
                out.workflow = Field::Known(Workflow::Obs);
                let (status, gate) = tokio::join!(read_status(ctx, &id), read_obs(ctx, &rrid));
                fill_obs(&mut out, &rrid, gate);
                out.template = template_of(&mut out, status, Commit::NotNeeded);
            }
        }
    }
    out
}

/// The document's pull request API URL, refused unless it is the RRID's.
fn pull_of(doc: &ReportDocument, rrid: &RequestReviewID) -> Result<String, String> {
    let api = doc
        .update
        .origin
        .api
        .as_deref()
        .ok_or("the published document names no pull request API URL")?;
    match pull_api(api) {
        Some((api, number)) if number == rrid.review_id => Ok(api),
        Some((_, number)) => Err(format!(
            "the published document names pull request {number}, not the RRID's {}",
            rrid.review_id
        )),
        None => Err("the published document's update.origin.api is not a pull request URL".into()),
    }
}

fn text(value: &str) -> String {
    one_line(value, Some(TEXT_LIMIT))
}

fn fill_pull(out: &mut UpdateReview, pull: Result<PullState, String>) {
    out.state = Field::NotApplicable;
    out.state_since = Field::NotApplicable;
    let pull = match pull {
        Ok(pull) => pull,
        Err(reason) => {
            out.fail(Source::Gitea, &reason);
            return;
        }
    };
    let (gate, since) = match pull.gate {
        PullReviewGate::None => (Gate::None, Field::None),
        PullReviewGate::Known(g) => (
            Gate::Known {
                state: match g.verdict {
                    PullVerdict::Owed => GateState::Owed,
                    PullVerdict::Approved => GateState::Approved,
                    PullVerdict::ChangesRequested => GateState::ChangesRequested,
                },
                stale: g.stale,
                closed: g.closed,
            },
            Field::known_or_unknown(g.since.as_deref().map(text)),
        ),
        PullReviewGate::Unknown(reason) => {
            out.fail(Source::Gitea, &reason);
            (Gate::Unknown, Field::Unknown)
        }
    };
    out.reviews = Field::Known(vec![GroupReview {
        group: text(&pull.group),
        automated: false,
        owner: match pull.owner.as_deref() {
            None => Owner::None,
            Some("") => Owner::Unknown,
            Some(login) => Owner::Known { login: text(login) },
        },
        gate,
        since,
    }]);
    out.created = Field::known_or_unknown(pull.created.as_deref().map(text));
    out.base = Field::known_or_unknown(pull.base.as_deref().map(text));
    out.head = Field::Known(text(&pull.head));
}

fn fill_obs(out: &mut UpdateReview, rrid: &RequestReviewID, gate: Result<RequestGate, String>) {
    out.head = Field::NotApplicable;
    out.base = Field::NotApplicable;
    let gate = match gate {
        Ok(gate) => gate,
        Err(reason) => {
            out.fail(Source::Obs, &reason);
            return;
        }
    };
    if rrid.kind == RequestKind::Maintenance {
        let expected = format!("SUSE:Maintenance:{}", rrid.maintenance_id);
        if gate.source_project.as_deref() != Some(expected.as_str()) {
            let actual = gate.source_project.as_deref().unwrap_or("absent");
            out.fail(
                Source::Obs,
                &format!(
                    "request {}'s source project is {actual}, not {expected}",
                    rrid.review_id
                ),
            );
            return;
        }
    }
    out.state = Field::known_or_unknown((!gate.state.is_empty()).then(|| text(&gate.state)));
    out.state_since = Field::known_or_unknown(gate.state_since.as_deref().map(text));
    out.created = Field::known_or_unknown(gate.created.as_deref().map(text));
    let mut reviews = Vec::new();
    for g in &gate.groups {
        let owner = match &g.owner {
            GroupOwner::Nobody => Owner::None,
            GroupOwner::User(user) => Owner::Known { login: text(user) },
            GroupOwner::Disputed { hold, history } => {
                let named = if history.is_empty() {
                    "nobody".to_owned()
                } else {
                    text(&history.join(", "))
                };
                out.notes.push(format!(
                    "{}: who holds {} is unclear: its open user review names {}, its review \
                     history {named}",
                    out.update,
                    text(&g.group),
                    hold.as_deref().map_or_else(|| "nobody".to_owned(), text),
                ));
                Owner::Disputed {
                    hold: hold.as_deref().map(text),
                    history: history.iter().map(|h| text(h)).collect(),
                }
            }
        };
        reviews.push(GroupReview {
            group: text(&g.group),
            automated: g.automated,
            owner,
            gate: Gate::Known {
                state: match g.verdict {
                    GroupVerdict::Open => GateState::New,
                    GroupVerdict::Assigned => GateState::Assigned,
                    GroupVerdict::Accepted => GateState::Accepted,
                },
                stale: false,
                closed: false,
            },
            since: Field::known_or_unknown(g.since.as_deref().map(text)),
        });
    }
    out.reviews = Field::Known(reviews);
}

fn template_of(
    out: &mut UpdateReview,
    status: Result<TemplateStatus, String>,
    commit: Commit,
) -> Template {
    let status = match status {
        Ok(status) => status,
        Err(reason) => {
            out.fail(Source::Teregen, &reason);
            return Template::Unknown;
        }
    };
    if status.generating() {
        return Template::Generating;
    }
    if !status.template {
        return Template::Missing {
            error: status
                .minion_error
                .as_deref()
                .and_then(|e| e.lines().map(str::trim).find(|l| !l.is_empty()))
                .map(text),
        };
    }
    match commit {
        Commit::NotNeeded => Template::Current,
        Commit::ServerStale => Template::Stale,
        Commit::Unknown => Template::Unknown,
        Commit::Compare { commit: None, .. } => {
            out.fail(
                Source::Document,
                "the published document names no update.origin.commit",
            );
            Template::Unknown
        }
        Commit::Compare { head: None, .. } => Template::Unknown,
        Commit::Compare {
            commit: Some(commit),
            head: Some(head),
        } => {
            if commit == head {
                Template::Current
            } else {
                Template::Stale
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        template: bool,
        state: Option<&str>,
        error: Option<&str>,
    ) -> Result<TemplateStatus, String> {
        Ok(TemplateStatus {
            template,
            minion_state: state.map(str::to_owned),
            minion_error: error.map(str::to_owned),
        })
    }

    #[test]
    fn template_states_and_labels() {
        let compare = |commit: &str, head: &str| Commit::Compare {
            commit: Some(commit.to_owned()),
            head: Some(head.to_owned()),
        };
        let mut out = UpdateReview::new("U".to_owned());
        let cases = [
            (status(true, None, None), compare("a", "a"), "current"),
            (status(true, None, None), compare("a", "b"), "stale"),
            (
                status(true, Some("active"), None),
                compare("a", "a"),
                "generating",
            ),
            (
                status(false, Some("failed"), Some("\n no build\nmore")),
                Commit::NotNeeded,
                "missing: no build",
            ),
            (status(false, None, None), Commit::NotNeeded, "missing"),
            (status(true, None, None), Commit::ServerStale, "stale"),
            (status(true, None, None), Commit::NotNeeded, "current"),
        ];
        for (status, commit, want) in cases {
            assert_eq!(template_of(&mut out, status, commit).to_string(), want);
        }
        assert!(out.failures.is_empty());
        assert_eq!(
            template_of(&mut out, Err("down".into()), Commit::NotNeeded),
            Template::Unknown
        );
        assert_eq!(out.failures[0].source, Source::Teregen);
        let no_commit = Commit::Compare {
            commit: None,
            head: Some("a".into()),
        };
        assert_eq!(
            template_of(&mut out, status(true, None, None), no_commit),
            Template::Unknown
        );
        assert_eq!(out.failures[1].source, Source::Document);
        assert_eq!(Template::NotApplicable.to_string(), "n/a");
        assert_eq!(Template::Unknown.to_string(), "?");
    }

    #[test]
    fn gate_labels() {
        let known = |state, stale, closed| Gate::Known {
            state,
            stale,
            closed,
        };
        let cases = [
            (known(GateState::Owed, false, false), "owed"),
            (known(GateState::Owed, false, true), "closed"),
            (known(GateState::Approved, false, false), "approved"),
            (known(GateState::Approved, true, false), "approved, stale"),
            (
                known(GateState::ChangesRequested, false, false),
                "changes-requested",
            ),
            (
                known(GateState::ChangesRequested, true, false),
                "changes-requested, stale",
            ),
            (known(GateState::New, false, false), "new"),
            (known(GateState::Assigned, false, false), "new (assigned)"),
            (known(GateState::Accepted, false, false), "accepted"),
            (Gate::None, "-"),
            (Gate::Unknown, "?"),
        ];
        for (gate, want) in cases {
            assert_eq!(gate.to_string(), want);
        }
    }

    #[test]
    fn field_and_owner_labels() {
        assert_eq!(Field::Known("x").to_string(), "x");
        assert_eq!(Field::<&str>::Unknown.to_string(), "?");
        assert_eq!(Field::<&str>::None.to_string(), "-");
        assert_eq!(Field::<&str>::NotApplicable.to_string(), "n/a");
        assert_eq!(Owner::None.to_string(), "-");
        assert_eq!(
            Owner::Disputed {
                hold: None,
                history: vec![]
            }
            .to_string(),
            "?"
        );
    }

    #[test]
    fn json_shape_is_tagged() {
        let gate = serde_json::to_value(Gate::Known {
            state: GateState::ChangesRequested,
            stale: true,
            closed: false,
        })
        .unwrap();
        assert_eq!(
            gate,
            serde_json::json!({"status": "known", "state": "changes_requested", "stale": true, "closed": false})
        );
        assert_eq!(
            serde_json::to_value(Field::Known("main")).unwrap(),
            serde_json::json!({"status": "known", "value": "main"})
        );
        assert_eq!(
            serde_json::to_value(Field::<String>::Unknown).unwrap(),
            serde_json::json!({"status": "unknown"})
        );
        assert_eq!(
            serde_json::to_value(Template::Missing { error: None }).unwrap(),
            serde_json::json!({"status": "missing", "error": null})
        );
    }

    #[test]
    fn pull_api_accepts_web_and_api_urls_only() {
        let api = "https://src.suse.de/api/v1/repos/products/SLFO/pulls/7".to_owned();
        assert_eq!(
            pull_api("https://src.suse.de/products/SLFO/pulls/7"),
            Some((api.clone(), 7))
        );
        assert_eq!(pull_api(&api), Some((api, 7)));
        assert_eq!(
            pull_api("https://u:p@h:8443/o/r/pulls/2"),
            Some(("https://h:8443/api/v1/repos/o/r/pulls/2".to_owned(), 2))
        );
        for bad in [
            "https://src.suse.de/products/SLFO/pulls/7/files",
            "https://src.suse.de/products/SLFO/issues/7",
            "https://src.suse.de/SLFO/pulls/7",
            "https://src.suse.de/products/SLFO/pulls/x",
            "ftp://src.suse.de/products/SLFO/pulls/7",
            "SUSE:Bogus:1:2",
        ] {
            assert_eq!(pull_api(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_refused_url_does_not_echo_its_credentials() {
        let message = parse_target("https://user:s3cret@h/o/r/issues/1").unwrap_err();
        assert!(message.starts_with("https://"), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
    }

    fn obs_review(gate: RequestGate) -> UpdateReview {
        let mut out = UpdateReview::new("U".to_owned());
        let rrid = RequestReviewID::parse("SUSE:PI:1.1:5").unwrap();
        fill_obs(&mut out, &rrid, Ok(gate));
        out
    }

    fn request_gate(created: Option<&str>, when: Option<&str>) -> RequestGate {
        RequestGate {
            state: "review".to_owned(),
            state_since: when.map(str::to_owned),
            created: created.map(str::to_owned),
            source_project: None,
            groups: vec![],
        }
    }

    /// A request without a state name is unknown, and its groups still show.
    /// A blank assignee from the marker replay is unknown, never a login.
    #[test]
    fn a_blank_gitea_owner_is_unknown() {
        let mut out = UpdateReview::new("U".to_owned());
        fill_pull(
            &mut out,
            Ok(PullState {
                group: "qam-sle".to_owned(),
                owner: Some(String::new()),
                gate: PullReviewGate::None,
                created: None,
                base: None,
                head: "abc".to_owned(),
            }),
        );
        let Field::Known(reviews) = &out.reviews else {
            panic!("{:?}", out.reviews);
        };
        assert_eq!(reviews[0].owner, Owner::Unknown);
        assert_eq!(reviews[0].owner.to_string(), "?");
        assert_eq!(
            serde_json::to_value(&reviews[0].owner).unwrap(),
            serde_json::json!({"status": "unknown"})
        );
    }

    #[test]
    fn a_blank_obs_state_is_unknown() {
        let mut gate = request_gate(Some("C"), Some("W"));
        gate.state = String::new();
        let out = obs_review(gate);
        assert_eq!(out.state, Field::Unknown);
        assert!(out.in_review());
    }

    /// An absent OBS time is unknown, never "nothing".
    #[test]
    fn absent_obs_times_are_unknown() {
        let out = obs_review(request_gate(None, None));
        assert_eq!(out.created, Field::Unknown);
        assert_eq!(out.state_since, Field::Unknown);
        let out = obs_review(request_gate(Some("C"), Some("W")));
        assert_eq!(out.created, Field::Known("C".to_owned()));
        assert_eq!(out.state_since, Field::Known("W".to_owned()));
    }
}
