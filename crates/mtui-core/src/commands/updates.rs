//! The `updates` command — list the update queue via the TeReGen API.

use std::collections::HashSet;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use futures::stream::{self, StreamExt};
use mtui_config::Config;
use mtui_datasources::gitea::token_login;
use mtui_datasources::obs::models::ObsComment;
use mtui_datasources::obs::{Osc, oscrc, qam};
use mtui_datasources::{GiteaError, HttpClient, UpdatesQuery, sanitize_url};
use mtui_types::{RequestKind, RequestReviewID};
use serde_json::Value;

use crate::command::{Command, Scope};
use crate::commands::apicall::{teregen_client, teregen_failure};
#[cfg(test)]
use crate::display::TRUNCATION_MARK;
use crate::display::sanitize_external;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

use super::row_budget::{add_window_args, crush, json_envelope, window};

/// Narrowing flags named in the row-budget notice.
const UPDATES_HINT: &str = "--limit/--offset/--field/-G";

/// Non-`testing` rows survive the crush; unknown/missing/null status keeps too (safe direction).
/// Severity: `failed`/`blocked` (2) outrank other non-`testing` (1).
fn anomaly_severity(v: &Value) -> u8 {
    match v.get("status").and_then(Value::as_str) {
        Some("testing") => 0,
        Some("failed" | "blocked") => 2,
        _ => 1,
    }
}

/// Lightweight dedup key: id/status/priority only, not the whole serialised row.
fn update_key(
    v: &Value,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let field = |k: &str| v.get(k).map(ToString::to_string);
    let id = v
        .get("id")
        .filter(|v| !v.is_null())
        .map(ToString::to_string);
    // Id-less rows have no stable identity: fall back to the full row there.
    let rest = id.is_none().then(|| v.to_string());
    (id, field("status"), field("priority"), rest)
}

/// The `--status` value that widens the queue to every status.
const STATUS_ALL: &str = "all";

/// Lists the update queue (unassigned + in-testing by default), fetched live
/// from the TeReGen API.
///
/// The default view is the actionable pickup queue: **unassigned** updates that
/// are **in testing**. `--assignee`/`--mine`/`--all-assignees` pick another
/// assignment view (dropping the unassigned default); `--status X` asks for
/// that status alone and `--status all` for every status. TeReGen scopes
/// assignment to testing updates, so the assignment views and the unassigned
/// default exist only for `--status testing`, and the views are refused with
/// any other status. Session-global, so [`Scope::Single`] rather than a per-template
/// fan-out — though it issues one TeReGen query per `--mine` login.
pub struct Updates;

#[async_trait]
impl Command for Updates {
    fn name(&self) -> &'static str {
        "updates"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Lists the update queue (unassigned + in-testing by default), fetched live from the TeReGen API.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Session-global query against the datasource.
        false
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        let cmd = cmd
            .arg(
                Arg::new("review_group")
                    .short('G')
                    .long("review-group")
                    .value_name("GROUP")
                    .action(ArgAction::Append)
                    .help(
                        "filter by review group, e.g. qam-sle or the \
                     '<group>-review' login form; repeatable — updates in any of \
                     the groups are listed (one server query)",
                    ),
            )
            .arg(
                Arg::new("field")
                    .short('F')
                    .long("field")
                    .value_name("FIELD")
                    .action(ArgAction::Append)
                    .help(
                        "select output fields by osc-qam name (e.g. -F Rating \
                     -F 'Assigned Roles'); repeatable, rendered as one block per \
                     update; names match case-insensitively, ignoring \
                     spaces/hyphens/underscores; Package-Streams/Issues/Comments \
                     are read live from OBS per shown row (after \
                     --limit/--offset), so narrow large queues with --limit; \
                     non-OBS (SLFO) rows render n/a, a row OBS refuses renders \
                     unavailable (an unreadable comments document marks \
                     Comments alone), comments are sanitised and capped per row",
                    ),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("field")
                    .help(
                        "print the raw TeReGen rows as a JSON array (each row \
                     emitted whole, unlike -F; honours --limit/--offset; not combinable \
                     with -F); an empty queue prints []; over-cap stdout is a JSON \
                     envelope {\"rows\": [...], \"truncation\": {...}} so stdout stays \
                     valid JSON",
                    ),
            )
            .arg(
                Arg::new("status")
                    .long("status")
                    .value_name("STATUS")
                    .default_value("testing")
                    .help(
                        "filter by status (default: testing); use 'all' for every status; \
                     the unassigned default and --assignee/--mine/--all-assignees \
                     apply to testing only",
                    ),
            );
        add_window_args(cmd)
        .arg(
            Arg::new("assignee")
                .long("assignee")
                .value_name("USER")
                .help("filter to updates assigned to this user (any qam group)"),
        )
        .arg(
            Arg::new("mine")
                .long("mine")
                .action(ArgAction::SetTrue)
                .help(
                    "filter to updates assigned to your oscrc login for [obs] api_url \
                     and, with a [gitea] token, its Gitea login",
                ),
        )
        .arg(
            Arg::new("all_assignees")
                .long("all-assignees")
                .action(ArgAction::SetTrue)
                .help(
                    "show every update regardless of assignee, overriding the unassigned default",
                ),
        )
        .group(
            ArgGroup::new("assignment")
                .args(["assignee", "mine", "all_assignees"])
                .multiple(false),
        )
    }

    fn complete(&self, _session: &Session, text: &str, line: &str) -> Vec<String> {
        // Right after `-F`/`--field`, offer the field names, not more flags.
        // Empty tokens are dropped: a trailing space would otherwise read as an
        // empty "previous token" and hide the names just as `-F ` was typed.
        let tokens: Vec<&str> = line.split(' ').filter(|t| !t.is_empty()).collect();
        let prev = if text.is_empty() {
            tokens.last().copied()
        } else {
            tokens
                .len()
                .checked_sub(2)
                .and_then(|i| tokens.get(i))
                .copied()
        };
        if matches!(prev, Some("-F" | "--field")) {
            if let Some(exact) = FIELDS.iter().find(|s| s.canonical == text) {
                return vec![exact.canonical.to_owned()];
            }
            return FIELDS
                .iter()
                .map(|s| s.canonical.to_owned())
                .filter(|c| c.starts_with(text))
                .collect();
        }
        // `-G`/`-F` are repeatable, so they ride in `extra`: a synonym group
        // would stop offering them once they first appear on the line.
        super::support::complete_choices(
            &[
                &["--status"],
                &["--limit"],
                &["--offset"],
                &["--assignee"],
                &["--mine"],
                &["--all-assignees"],
                &["--json"],
            ],
            ["-G", "--review-group", "-F", "--field"]
                .map(str::to_owned)
                .to_vec(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // A repeated `-G` is sent once.
        let review_groups: Vec<String> = {
            let mut seen = HashSet::new();
            args.get_many::<String>("review_group")
                .map(|v| v.filter(|g| seen.insert((*g).clone())).cloned().collect())
                .unwrap_or_default()
        };
        // Resolve every -F name up front so a typo errors before any query.
        let specs = args
            .get_many::<String>("field")
            .map(|v| v.map(|f| resolve_field(f)).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        let needs_obs = specs.iter().any(|s| s.needs_obs);
        let as_json = args.get_flag("json");
        let status_arg = args
            .get_one::<String>("status")
            .cloned()
            .unwrap_or_else(|| "testing".to_owned());
        let all_assignees = args.get_flag("all_assignees");
        if status_arg != "testing"
            && (all_assignees || args.get_flag("mine") || args.contains_id("assignee"))
        {
            return Err(CommandError::Other(
                "--assignee/--mine/--all-assignees apply to --status testing only: TeReGen \
                 serves assignment for updates in testing; drop the assignment flag or use \
                 --status testing"
                    .to_owned(),
            ));
        }

        let (assignees, note) = if args.get_flag("mine") {
            // oscrc errors name local files, so headlessly only the fault.
            let principal = oscrc::read_user(&session.config.obs_api_url).map_err(|e| {
                if session.is_repl {
                    e.to_string()
                } else {
                    e.fault().to_owned()
                }
            });
            let gitea = if session.config.gitea_token.is_empty() {
                None
            } else {
                let token = session.config.gitea_token.clone();
                let gitea_url = session.config.gitea_url.clone();
                Some(match session.http_client() {
                    Ok(http) => token_login(http, &token, &gitea_url).await,
                    Err(e) => Err(e.into()),
                })
            };
            let mine = mine_logins(&session.config.obs_api_url, principal, gitea)?;
            if as_json && let Some(note) = &mine.note {
                return Err(CommandError::Other(format!(
                    "{note}; --json cannot carry that note, so pass --assignee <login> instead"
                )));
            }
            (mine.logins, mine.note)
        } else {
            let assignee = args.get_one::<String>("assignee").cloned();
            (assignee.into_iter().collect::<Vec<_>>(), None)
        };

        // '--status all' widens by sending no status filter at all.
        let status_all = status_arg == STATUS_ALL;
        let status = if status_all { None } else { Some(status_arg) };

        // Assignment is scoped to testing server-side: sending any assignment
        // param forces `status=testing`, so any other status asks for none.
        // The default testing view is the unassigned pickup queue, which
        // --assignee/--mine/--all-assignees opt out of.
        let is_testing = status.as_deref() == Some("testing");
        let unassigned = is_testing && assignees.is_empty() && !all_assignees;

        // Without `with_assignment` TeReGen omits assignee/assignees on SLFO
        // rows, so an assignment-derived field would render a false
        // "unassigned".
        if !is_testing && let Some(spec) = specs.iter().find(|s| s.needs_assignment) {
            return Err(CommandError::Other(format!(
                "field '{}' needs assignment data, which TeReGen serves for \
                 --status testing only; use --status testing",
                spec.canonical
            )));
        }

        let teregen = teregen_client(session)?;
        // One query per assignee (none = one unfiltered query), carrying every
        // `-G` group: TeReGen serves the union of a repeated `review_group`.
        let groups: Vec<&str> = review_groups.iter().map(String::as_str).collect();
        let status_filter = status.as_deref();
        let assignee_filters: Vec<Option<&str>> = if assignees.is_empty() {
            vec![None]
        } else {
            assignees.iter().map(|a| Some(a.as_str())).collect()
        };
        let queries: Vec<UpdatesQuery<'_>> = assignee_filters
            .iter()
            .map(|a| UpdatesQuery {
                review_groups: &groups,
                status: status_filter,
                assignee: *a,
                unassigned,
                with_assignment: is_testing,
                no_cache: false,
            })
            .collect();
        let multi = queries.len() > 1;
        // `buffered` keeps batches in login order. Materialised eagerly: a lazy
        // `map` closure trips a higher-ranked-lifetime limit.
        let futures: Vec<_> = queries.iter().map(|q| teregen.updates(q)).collect();
        let cancel = session.cancel_token();
        let results: Vec<_> = tokio::select! {
            () = cancel.cancelled() => {
                return Err(CommandError::Cancelled(
                    "updates cancelled while waiting for TeReGen".to_owned(),
                ));
            }
            results = stream::iter(futures).buffered(4).collect() => results,
        };

        // `Err` (transport/API failure) is surfaced, `Ok(None)` (no `updates`
        // key) skipped: only a *successful* empty result is an empty queue.
        let mut rows: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for result in results {
            let batch = result.map_err(|e| {
                CommandError::Other(format!(
                    "Update queue query failed: {}",
                    teregen_failure(&e)
                ))
            })?;
            let Some(batch) = batch else { continue };
            let batch = batch.as_array().ok_or_else(|| {
                CommandError::Other("Update queue query returned a malformed response".to_owned())
            })?;
            for row in batch {
                // A row can sit in several groups, or under both `--mine`
                // logins. One without an id is kept, never silently dropped.
                // A single query cannot repeat a row.
                let fresh = !multi
                    || match row.get("id").and_then(Value::as_str) {
                        Some(id) => seen.insert(id.to_owned()),
                        None => true,
                    };
                if fresh {
                    rows.push(row.clone());
                }
            }
        }
        if multi {
            // Restore the server's priority-descending order across the merge
            // (stable, so ties keep server order).
            rows.sort_by_key(|r| {
                std::cmp::Reverse(
                    r.get("priority")
                        .and_then(Value::as_i64)
                        .unwrap_or(i64::MIN),
                )
            });
        }

        if let Some(note) = &note {
            session.display.println(&format!("note: {note}"));
        }
        if rows.is_empty() {
            // Keep --json output parseable even for an empty queue.
            let empty = if as_json {
                "[]".to_owned()
            } else if assignees.is_empty() {
                "No updates in the queue".to_owned()
            } else {
                format!(
                    "No updates assigned to {} in the queue",
                    assignees.join(" or ")
                )
            };
            session.display.println(&empty);
            return Ok(());
        }

        // Paging via --offset (pre-crush) makes any middle slice recoverable; chosen over an
        // explicit-window notice as it fits the existing --limit plumbing.
        let windowed = window(args, rows);
        // Row budget backstops `--limit 0=all`: head+tail+anomalies, exact-deduped.
        // Row-cap is not byte-cap: MCP max_output_bytes can still cut mid-array on huge rows.
        let crushed = crush(windowed, update_key, anomaly_severity);
        let notice = crushed.notice(UPDATES_HINT);
        let meta = crushed.meta(UPDATES_HINT);
        let mut shown = crushed.kept;

        if as_json {
            // Over-cap --json stays in-band like the byte-cap convention, but as a
            // structured envelope so stdout stays valid JSON for jq/MCP parsers.
            if let Some(meta) = meta {
                session
                    .display
                    .println(&json_envelope(shown, Some(meta), None));
            } else {
                let doc = Value::Array(shown);
                session.display.println(
                    &serde_json::to_string_pretty(&doc)
                        .expect("serialising a serde_json::Value is infallible"),
                );
            }
            return Ok(());
        }

        if crushed.truncated > 0 {
            session.display.println(&format!(
                "Update queue ({} of {}):",
                shown.len(),
                crushed.total
            ));
        } else {
            session
                .display
                .println(&format!("Update queue ({}):", shown.len()));
        }
        if specs.is_empty() {
            for u in &shown {
                session.display.println(&render_row(u, is_testing));
            }
        } else {
            if needs_obs {
                // The transport builds only when a shown row can be looked up:
                // an all-idless (or all-non-OBS) listing answers without
                // credentials. Non-OBS rows are stamped first so their
                // fields render n/a rather than dashes.
                let (targets, _non_obs) = partition_obs_rows(&mut shown);
                if targets.is_empty() {
                    tracing::debug!(
                        "OBS enrichment skipped: no shown row has a parseable RequestReviewID"
                    );
                } else {
                    // The transport builds only for a lookup that will run;
                    // `Session` is `!Sync`, so the call below takes owned
                    // values and borrows nothing across its awaits.
                    let http = session.http_client().map_err(|e| {
                        CommandError::Other(format!("could not build OBS client: {e}"))
                    })?;
                    let config = session.config.clone();
                    enrich_shown_with_obs(config, http, &mut shown, targets).await?;
                }
            }
            for (i, u) in shown.iter().enumerate() {
                if i > 0 {
                    session.display.println("");
                }
                session.display.println(&render_fields(u, &specs));
            }
        }
        if let Some(notice) = notice {
            session.display.println(&notice);
        }
        Ok(())
    }
}

/// The logins `--mine` asks about, and a note naming the login source that
/// failed, if any.
struct MineLogins {
    logins: Vec<String>,
    note: Option<String>,
}

/// `--mine` asks about the logins assignments are recorded under: the oscrc
/// `user` for `obs_api_url` (`principal`), which OBS review actions record,
/// and the Gitea token owner's login (`gitea`, `None` without a token), which
/// Gitea markers record. Either alone still answers for the updates recorded
/// under it, so a failed one becomes a note; with neither there is nobody to
/// ask about.
fn mine_logins(
    obs_api_url: &str,
    principal: Result<String, String>,
    gitea: Option<Result<String, GiteaError>>,
) -> Result<MineLogins, CommandError> {
    let no_principal = |reason: String| {
        format!(
            "the OBS login (the oscrc user for [{}]) could not be read: {reason}",
            sanitize_url(obs_api_url)
        )
    };
    let no_gitea = |e: GiteaError| {
        format!("the Gitea login (the [gitea] token's owner) could not be resolved: {e}")
    };
    let only = |login: String, why: String| MineLogins {
        note: Some(format!("{why}; listing only updates assigned to {login}")),
        logins: vec![login],
    };
    match (principal, gitea) {
        (Ok(principal), None) => Ok(MineLogins {
            logins: vec![principal],
            note: None,
        }),
        (Ok(principal), Some(Ok(login))) => {
            let mut logins = vec![principal];
            if !logins.contains(&login) {
                logins.push(login);
            }
            Ok(MineLogins { logins, note: None })
        }
        (Ok(principal), Some(Err(e))) => Ok(only(principal, no_gitea(e))),
        (Err(reason), Some(Ok(login))) => Ok(only(login, no_principal(reason))),
        (Err(reason), None) => Err(CommandError::Other(format!(
            "--mine has no login to ask about: {}, and no [gitea] token is set; \
             fix the oscrc or pass --assignee <login>",
            no_principal(reason)
        ))),
        (Err(reason), Some(Err(e))) => Err(CommandError::Other(format!(
            "--mine has no login to ask about: {}, and {}; pass --assignee <login>",
            no_principal(reason),
            no_gitea(e)
        ))),
    }
}

/// One selectable `-F` output field: the canonical osc-qam spelling plus its
/// extraction from a TeReGen queue row.
struct FieldSpec {
    /// Canonical display name, as `osc qam list -F` spells it.
    canonical: &'static str,
    /// Extra accepted spellings (pre-normalized), beyond the canonical name.
    aliases: &'static [&'static str],
    /// Whether the value only exists when the query sends `with_assignment`.
    /// Requesting it in a view that cannot carry assignment is refused rather
    /// than blanked: the omission would render a false "unassigned".
    needs_assignment: bool,
    /// Whether the value is read live from OBS per shown row instead of the
    /// TeReGen queue listing. Requesting one fans out over the shown rows
    /// after `--limit`/`--offset`, building the OBS client only when a shown
    /// row can be looked up.
    needs_obs: bool,
    extract: fn(&serde_json::Map<String, Value>) -> String,
}

/// The selectable fields. osc-qam names are canonical
/// so `osc qam list -F` muscle memory transfers; the raw TeReGen key is an
/// alias where it differs. Most extract from the TeReGen queue row;
/// `Package-Streams`/`Issues`/`Comments` are read live from OBS per shown row.
static FIELDS: &[FieldSpec] = &[
    FieldSpec {
        canonical: "ReviewRequestID",
        aliases: &["id"],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "id"),
    },
    FieldSpec {
        canonical: "Incident Priority",
        aliases: &["priority", "prio"],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "priority"),
    },
    FieldSpec {
        canonical: "Rating",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "rating"),
    },
    FieldSpec {
        canonical: "Category",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "category"),
    },
    FieldSpec {
        canonical: "Status",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "status"),
    },
    FieldSpec {
        canonical: "Kind",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "kind"),
    },
    FieldSpec {
        canonical: "Deadline",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "deadline"),
    },
    FieldSpec {
        canonical: "Assignee",
        aliases: &[],
        needs_assignment: true,
        needs_obs: false,
        extract: |o| {
            o.get("assignee")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unassigned")
                .to_owned()
        },
    },
    FieldSpec {
        canonical: "Assigned Roles",
        aliases: &[],
        needs_assignment: true,
        needs_obs: false,
        extract: extract_assigned_roles,
    },
    FieldSpec {
        canonical: "Unassigned Roles",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: extract_unassigned_roles,
    },
    FieldSpec {
        canonical: "Title",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "title"),
    },
    FieldSpec {
        canonical: "URL",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "url"),
    },
    FieldSpec {
        canonical: "Products",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| string_list_field(o, "products"),
    },
    FieldSpec {
        canonical: "SRCRPMs",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| string_list_field(o, "srcrpms"),
    },
    FieldSpec {
        canonical: "Bugs",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| string_list_field(o, "bugs"),
    },
    FieldSpec {
        canonical: "Creator",
        aliases: &[],
        needs_assignment: false,
        needs_obs: false,
        extract: |o| scalar_field(o, "creator"),
    },
    FieldSpec {
        // Not TeReGen's `codestreams` under another key: osc-qam derives
        // Package-Streams from the OBS request's `src_package` set (bare
        // names like `glibc`), while `codestreams` carries project
        // identifiers (`SUSE:SLE-15-SP2:Update`, `SUSE:SLFO:1.2`), so the
        // extractor reads only the OBS-injected `obs_package_streams` key.
        canonical: "Package-Streams",
        aliases: &[],
        needs_assignment: false,
        needs_obs: true,
        extract: |o| obs_string_list_field(o, "obs_package_streams"),
    },
    FieldSpec {
        canonical: "Issues",
        aliases: &[],
        needs_assignment: false,
        needs_obs: true,
        extract: |o| obs_string_list_field(o, "obs_issues"),
    },
    FieldSpec {
        canonical: "Comments",
        aliases: &[],
        needs_assignment: false,
        needs_obs: true,
        extract: extract_obs_comments,
    },
];

/// Normalizes a field name for matching: lowercase, separators dropped, so
/// `Incident Priority`, `incident-priority` and `incident_priority` coincide.
fn normalize_field(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolves a `-F` name to its spec, or an error listing what is available.
fn resolve_field(name: &str) -> Result<&'static FieldSpec, CommandError> {
    let norm = normalize_field(name);
    if let Some(spec) = FIELDS
        .iter()
        .find(|s| normalize_field(s.canonical) == norm || s.aliases.iter().any(|a| *a == norm))
    {
        return Ok(spec);
    }
    Err(CommandError::Other(format!(
        "unknown field '{name}'; available fields: {}",
        available_field_names()
    )))
}

/// The canonical names of every servable field, comma-joined for error text.
fn available_field_names() -> String {
    FIELDS
        .iter()
        .map(|s| s.canonical)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A scalar row key as display text: missing/null → `-`.
fn scalar_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    match obj.get(key) {
        None | Some(Value::Null) => "-".to_owned(),
        Some(v) => json_scalar(v),
    }
}

/// A string-list row key as display text: missing/null/non-array/empty → `-`.
/// Non-string elements are skipped and empty strings dropped.
fn string_list_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    let Some(arr) = obj.get(key).and_then(Value::as_array) else {
        return "-".to_owned();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(", ")
    }
}

/// `Comments`: the OBS-injected comment lines, newline-joined with indented
/// continuations (keeping `render_fields`' one-`Name:`-per-block shape);
/// an explicit null (a deliberately skipped non-OBS row) renders `n/a`;
/// the [`OBS_UNAVAILABLE`] sentinel (an unreadable OBS read) renders
/// `unavailable`; missing/non-array/empty → `-`. Non-string elements are
/// skipped.
fn extract_obs_comments(obj: &serde_json::Map<String, Value>) -> String {
    if obj.get("obs_comments").is_some_and(Value::is_null) {
        return "n/a".to_owned();
    }
    if obs_unavailable(obj, "obs_comments") {
        return OBS_UNAVAILABLE.to_owned();
    }
    let Some(arr) = obj.get("obs_comments").and_then(Value::as_array) else {
        return "-".to_owned();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join("\n  ")
    }
}

/// `Assigned Roles`: flattens TeReGen's `assignees` map
/// (`{group: [{state, user}]}`) to `group:user` pairs; a non-`assigned` state
/// is kept visible as `group:user(state)`.
fn extract_assigned_roles(obj: &serde_json::Map<String, Value>) -> String {
    let Some(map) = obj.get("assignees").and_then(Value::as_object) else {
        return "-".to_owned();
    };
    let mut parts = Vec::new();
    for (group, entries) in map {
        for entry in entries.as_array().into_iter().flatten() {
            let user = entry.get("user").and_then(Value::as_str).unwrap_or("?");
            match entry.get("state").and_then(Value::as_str) {
                Some("assigned") | None => parts.push(format!("{group}:{user}")),
                Some(state) => parts.push(format!("{group}:{user}({state})")),
            }
        }
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(", ")
    }
}

/// `Unassigned Roles`: `review_groups` minus the groups in `assignees`. A row
/// without `review_groups` has no role data, so the honest answer is `n/a`, not
/// an empty list.
fn extract_unassigned_roles(obj: &serde_json::Map<String, Value>) -> String {
    let Some(groups) = obj.get("review_groups").and_then(Value::as_array) else {
        return "n/a".to_owned();
    };
    // An absent `assignees` key means no assignment data, indistinguishable
    // from "nobody assigned", so `n/a` beats a list claiming every group is
    // open. Live the two keys travel together; the guard is for the day
    // they don't.
    let Some(assignee_map) = obj.get("assignees").and_then(Value::as_object) else {
        return "n/a".to_owned();
    };
    // A `{"qam-sle": []}` group has nobody on it: it must stay unassigned, not
    // vanish from both role lists.
    let assigned: HashSet<&str> = assignee_map
        .iter()
        .filter(|(_, entries)| entries.as_array().is_some_and(|a| !a.is_empty()))
        .map(|(group, _)| group.as_str())
        .collect();
    let open: Vec<&str> = groups
        .iter()
        .filter_map(Value::as_str)
        .filter(|g| !assigned.contains(g))
        .collect();
    if open.is_empty() {
        "-".to_owned()
    } else {
        open.join(", ")
    }
}

/// Renders one queue row as a `-F` block: one `  Name: value` line per
/// requested field, in the order requested.
fn render_fields(u: &Value, specs: &[&'static FieldSpec]) -> String {
    let Some(obj) = u.as_object() else {
        return format!("  {u}");
    };
    specs
        .iter()
        .map(|s| format!("  {}: {}", s.canonical, (s.extract)(obj)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The OBS-backed list extractor: an explicit null (a deliberately skipped
/// non-OBS row, see [`partition_obs_rows`]) renders `n/a`; the
/// [`OBS_UNAVAILABLE`] sentinel (a row OBS refused, or — for `obs_comments` —
/// an unreadable comments document) renders `unavailable`; a missing key (an
/// id-less row) renders `-` like the TeReGen-backed lists, as does an empty
/// array (looked, found nothing). Every element goes through
/// [`sanitize_external`]: package names and issue refs are OBS text like
/// comments, and a raw ESC/C1 in either would reach the REPL as a live
/// terminal sequence. Kept separate from [`string_list_field`] so TeReGen
/// nulls keep their dash and TeReGen text keeps its verbatim rendering.
fn obs_string_list_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    if obj.get(key).is_some_and(Value::is_null) {
        return "n/a".to_owned();
    }
    if obs_unavailable(obj, key) {
        return OBS_UNAVAILABLE.to_owned();
    }
    let Some(arr) = obj.get(key).and_then(Value::as_array) else {
        return "-".to_owned();
    };
    let parts: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(|s| sanitize_external(s, Some(OBS_COMMENT_LIMIT)))
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Kept characters of each OBS text part (comment `who`/`when`/body, package
/// and issue strings): comments are the least-trusted text mtui prints (any
/// OBS user can write one), so each part goes through [`sanitize_external`]
/// like a checkers row (`checkers.rs::ROW_LIMIT`), cut to the same width.
const OBS_COMMENT_LIMIT: usize = 200;

/// Rendered for an OBS-backed field whose read failed: a row OBS refused, or
/// (for Comments only) an unreadable comments document. Distinct from `-`
/// (looked, found nothing) and `n/a` (SLFO, never looked up), so an MCP
/// client can tell a broken OBS read from an empty one. Stored as a bare
/// string under the `obs_*` key (success stores an array, skipped non-OBS
/// rows store null), recognised by [`obs_unavailable`].
const OBS_UNAVAILABLE: &str = "unavailable";

/// Whether an OBS-backed key carries the [`OBS_UNAVAILABLE`] sentinel.
fn obs_unavailable(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .is_some_and(|v| v.as_str() == Some(OBS_UNAVAILABLE))
}

/// Stamp the [`OBS_UNAVAILABLE`] sentinel on a row OBS refused, so the
/// OBS-backed extractors render `unavailable` instead of a dash that would
/// read as "looked, found nothing".
fn mark_obs_unavailable(row: &mut Value) {
    if let Some(obj) = row.as_object_mut() {
        for key in ["obs_package_streams", "obs_issues", "obs_comments"] {
            obj.insert(key.to_owned(), Value::String(OBS_UNAVAILABLE.to_owned()));
        }
    }
}

/// Comments kept per row: the row budget counts rows, not bytes, so an
/// unbounded history would blow past it. The tail is the live discussion;
/// the omitted count keeps the cut honest.
const OBS_COMMENTS_PER_ROW: usize = 5;

/// One OBS comment as a single `-F` line: `who (when): body`, naming only the
/// parts OBS served; the body is flattened (a comment's own newlines would
/// break the one-line-per-field block). An empty body leaves no dangling
/// separator. Every part is sanitised: a raw ESC/C1 in the body would reach
/// the REPL as a live terminal sequence, bidi overrides would reorder the
/// line, and over MCP the text lands in model context.
fn format_obs_comment(comment: &ObsComment) -> String {
    let body = sanitize_external(
        &comment
            .body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        Some(OBS_COMMENT_LIMIT),
    );
    let who = sanitize_external(&comment.who, Some(OBS_COMMENT_LIMIT));
    let when = sanitize_external(&comment.when, Some(OBS_COMMENT_LIMIT));
    let head = match (who.is_empty(), when.is_empty()) {
        (true, true) => String::new(),
        (false, true) => who,
        (true, false) => format!("({when})"),
        (false, false) => format!("{who} ({when})"),
    };
    match (head.is_empty(), body.is_empty()) {
        (_, true) => head,
        (true, false) => body,
        (false, false) => format!("{head}: {body}"),
    }
}

/// Merge one row's OBS fallback into its TeReGen row under dedicated `obs_`
/// keys the OBS-backed extractors read. Namespaced so a TeReGen row carrying
/// those bare names can neither satisfy the fields nor be overwritten.
/// Comments keep only the newest [`OBS_COMMENTS_PER_ROW`] by `when`, headed
/// by a marker counting the omitted ones. The tail is taken in `when` order
/// (stable, so equal timestamps keep document order) rather than document
/// order: OBS serves comments oldest-first, but nothing pins that, and `when`
/// is ISO-8601 so string order is chronological whatever order the document
/// arrived in. An unreadable comments document (`comments_unavailable`)
/// stores the [`OBS_UNAVAILABLE`] sentinel for `obs_comments` while the
/// packages and issues from the served request document still print.
fn inject_extras(row: &mut Value, extras: &qam::RequestExtras) {
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    let strings =
        |items: &[String]| Value::Array(items.iter().map(|s| Value::String(s.clone())).collect());
    obj.insert(
        "obs_package_streams".to_owned(),
        strings(&extras.src_packages),
    );
    obj.insert("obs_issues".to_owned(), strings(&extras.issues));
    if extras.comments_unavailable {
        obj.insert(
            "obs_comments".to_owned(),
            Value::String(OBS_UNAVAILABLE.to_owned()),
        );
        return;
    }
    let mut by_when: Vec<&ObsComment> = extras.comments.iter().collect();
    by_when.sort_by(|a, b| a.when.cmp(&b.when));
    let total = by_when.len();
    let tail = if total > OBS_COMMENTS_PER_ROW {
        &by_when[total - OBS_COMMENTS_PER_ROW..]
    } else {
        &by_when[..]
    };
    let mut comments = Vec::with_capacity(tail.len() + 1);
    let omitted = total - tail.len();
    if omitted > 0 {
        comments.push(Value::String(if omitted == 1 {
            "…[1 earlier comment omitted]".to_owned()
        } else {
            format!("…[{omitted} earlier comments omitted]")
        }));
    }
    comments.extend(tail.iter().map(|c| Value::String(format_obs_comment(c))));
    obj.insert("obs_comments".to_owned(), Value::Array(comments));
}

/// Whether a queue row's RRID names an OBS request: classic Maintenance and
/// PI review ids are OBS request ids (every backend command reads them as
/// `request/{review_id}`), while SLFO review ids are Gitea PR numbers — every
/// SLFO update is a Gitea PR (`docs/src/architecture.md`), and the RRID's
/// shape cannot tell the two apart. An OBS read of a PR number would serve an
/// unrelated request, so unknown future kinds fail closed to `false`.
fn obs_addressable(rrid: &RequestReviewID) -> bool {
    matches!(rrid.kind, RequestKind::Maintenance | RequestKind::Pi)
}

/// Split the shown rows in one pass: `(targets, non_obs)`, where `targets`
/// is `(index, RRID)` for rows with a parseable, OBS-addressable `id` in
/// display order, and `non_obs` is the indices of rows with a parseable but
/// non-OBS `id` (SLFO Gitea PR numbers, not OBS request ids — one row's
/// lookup must never serve another request's data). Non-OBS rows are stamped
/// null on the way through, so the OBS-backed extractors render `n/a` (like
/// `Unassigned Roles`) instead of a dash that would read as "looked, found
/// nothing". Unknown future kinds fail closed to `non_obs`: never looked up,
/// rendered `n/a` like SLFO rather than risking an unrelated request's data.
fn partition_obs_rows(shown: &mut [Value]) -> (Vec<(usize, RequestReviewID)>, Vec<usize>) {
    let mut targets = Vec::new();
    let mut non_obs = Vec::new();
    for (i, row) in shown.iter_mut().enumerate() {
        let Some(rrid) = row
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| RequestReviewID::parse(id).ok())
        else {
            continue;
        };
        if obs_addressable(&rrid) {
            targets.push((i, rrid));
        } else {
            if let Some(obj) = row.as_object_mut() {
                for key in ["obs_package_streams", "obs_issues", "obs_comments"] {
                    obj.insert(key.to_owned(), Value::Null);
                }
            }
            non_obs.push(i);
        }
    }
    (targets, non_obs)
}

/// Fill the OBS-backed `-F` keys into each shown row, read live from OBS after
/// `--limit`/`--offset` (and the row-budget crush) so a large queue costs one
/// fetch per *shown* row, not per queued row. A row OBS refuses keeps its
/// RRID but marks all three OBS-backed fields `unavailable` (distinct from
/// `-` for "looked, found nothing" and `n/a` for SLFO rows, so an MCP client
/// can tell a broken OBS read from an empty one); when *no* shown row is
/// servable the command errors (a silent all-dash listing would hide a dead
/// OBS base).
async fn enrich_shown_with_obs(
    config: Config,
    http: HttpClient,
    shown: &mut [Value],
    targets: Vec<(usize, RequestReviewID)>,
) -> Result<(), CommandError> {
    let (indices, rrids): (Vec<usize>, Vec<RequestReviewID>) = targets.into_iter().unzip();
    let total = rrids.len();
    let results = Osc::extras_for_many(&config, http, &rrids)
        .await
        .map_err(|e| CommandError::Other(format!("could not build OBS client: {e}")))?;
    let mut failed = 0;
    let mut first_err = String::new();
    for ((i, rrid), result) in indices.into_iter().zip(rrids).zip(results) {
        match result {
            Ok(extras) => inject_extras(&mut shown[i], &extras),
            Err(e) => {
                failed += 1;
                if first_err.is_empty() {
                    first_err = e.to_string();
                }
                tracing::warn!("OBS details for shown update {rrid} unreadable: {e}");
                mark_obs_unavailable(&mut shown[i]);
            }
        }
    }
    if failed == total {
        return Err(CommandError::Other(format!(
            "OBS details unavailable for all {total} shown rows ({first_err}); \
             check [obs] api_url and the oscrc credentials"
        )));
    }
    Ok(())
}

/// Renders one queue row in a fixed-width layout.
fn render_row(u: &serde_json::Value, want_assignment: bool) -> String {
    let Some(obj) = u.as_object() else {
        return format!("  {u}");
    };
    let field = |k: &str| {
        obj.get(k)
            .map(json_scalar)
            .unwrap_or_else(|| "?".to_owned())
    };
    // The date part of the ISO timestamp is enough. Stringify first so shape
    // drift shows its raw form rather than vanishing the row.
    let deadline = obj.get("deadline").filter(|v| !is_falsy(v)).map_or_else(
        || "-".to_owned(),
        |v| json_scalar(v).chars().take(10).collect(),
    );

    let mut row = format!(
        "  prio={:<5} {:<10} {:<12} {:<11} {}",
        field("priority"),
        field("status"),
        field("kind"),
        deadline,
        field("id"),
    );
    if want_assignment {
        let assignee = obj
            .get("assignee")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unassigned");
        row.push_str(&format!(" assignee={assignee}"));
    }
    row
}

/// Whether a JSON value is "falsy": null, `false`, numeric zero, or the empty
/// string. Used to collapse an absent/blank `deadline` to `-`.
fn is_falsy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::Bool(b) => !b,
        serde_json::Value::Number(n) => n.as_f64() == Some(0.0),
        serde_json::Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Renders a JSON scalar as a plain string.
fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "?".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};
    use mtui_config::Config;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Updates.name(), "updates");
        assert_eq!(Updates.scope(), Scope::Single);
    }

    #[test]
    fn complete_offers_static_flags() {
        let (session, _buf) = empty_session();
        let all = Updates.complete(&session, "", "updates ");
        for f in [
            "--review-group",
            "-G",
            "--status",
            "--limit",
            "--offset",
            "--assignee",
            "--mine",
            "--all-assignees",
            "--field",
            "-F",
            "--json",
        ] {
            assert!(all.contains(&f.to_owned()), "missing {f}: {all:?}");
        }
        assert_eq!(
            Updates.complete(&session, "--r", "updates --r"),
            vec!["--review-group"]
        );
    }

    #[test]
    fn complete_still_offers_repeatable_flags_after_first_use() {
        // Repeatable flags must survive prior use on the line; a synonym
        // group would suppress them once typed.
        let (session, _buf) = empty_session();
        assert_eq!(
            Updates.complete(&session, "--r", "updates --review-group qam-sle --r"),
            vec!["--review-group"]
        );
        let after_field = Updates.complete(&session, "-", "updates -F Rating -");
        assert!(after_field.contains(&"-F".to_owned()), "{after_field:?}");
        // A once-only flag IS suppressed after use.
        let after_json = Updates.complete(&session, "--j", "updates --json --j");
        assert!(after_json.is_empty(), "{after_json:?}");
    }

    #[test]
    fn complete_offers_field_names_after_field_flag() {
        let (session, _buf) = empty_session();
        // Bare -F: every canonical name, in registry order.
        let names = Updates.complete(&session, "", "updates -F ");
        assert_eq!(names.len(), FIELDS.len(), "{names:?}");
        assert_eq!(names[0], "ReviewRequestID");
        assert!(names.contains(&"Assigned Roles".to_owned()), "{names:?}");
        for name in ["Package-Streams", "Issues", "Comments"] {
            assert!(names.contains(&name.to_owned()), "{names:?}");
        }
        assert_eq!(
            Updates.complete(&session, "Rat", "updates -F Rat"),
            vec!["Rating"]
        );
        assert_eq!(
            Updates.complete(&session, "Cat", "updates --field Cat"),
            vec!["Category"]
        );
        // Not right after -F, flags come back, field names don't.
        let flags = Updates.complete(&session, "", "updates -F Rating ");
        assert!(!flags.contains(&"Category".to_owned()), "{flags:?}");
        assert!(flags.contains(&"--status".to_owned()), "{flags:?}");
    }

    #[test]
    fn assignment_flags_are_mutually_exclusive() {
        let base = clap::Command::new("updates").no_binary_name(true);
        let cmd = Updates.configure(base);
        assert!(
            cmd.clone()
                .try_get_matches_from(["--mine", "--all-assignees"])
                .is_err()
        );
        assert!(cmd.try_get_matches_from(["--mine"]).is_ok());
    }

    #[test]
    fn heavy_output_helps_point_at_limit() {
        // Token nudge: the unbounded-output flags must name the narrowing knob.
        let base = clap::Command::new("updates").no_binary_name(true);
        let cmd = Updates.configure(base);
        for id in ["field", "json"] {
            let help = cmd
                .get_arguments()
                .find(|a| a.get_id() == id)
                .and_then(|a| a.get_help())
                .map(|h| h.to_string())
                .unwrap_or_default();
            assert!(
                help.contains("--limit"),
                "{id} help lacks the narrowing hint: {help}"
            );
        }
    }

    #[test]
    fn render_row_includes_assignee_only_when_wanted() {
        let u = serde_json::json!({
            "priority": 3, "status": "testing", "kind": "Maintenance",
            "deadline": "2026-07-10T00:00:00", "id": "SUSE:Maintenance:1:1",
            "assignee": "alice"
        });
        let with = render_row(&u, true);
        assert!(with.contains("prio=3"), "{with}");
        assert!(with.contains("2026-07-10"), "{with}");
        assert!(with.contains("assignee=alice"), "{with}");
        let without = render_row(&u, false);
        assert!(!without.contains("assignee="), "{without}");
    }

    #[test]
    fn render_row_unassigned_and_missing_deadline() {
        let u = serde_json::json!({
            "priority": 1, "status": "testing", "kind": "SLFO",
            "id": "SUSE:SLFO:1.2:5"
        });
        let row = render_row(&u, true);
        assert!(row.contains("assignee=unassigned"), "{row}");
        assert!(
            row.contains(" - "),
            "missing deadline should render '-': {row}"
        );
    }

    #[test]
    fn render_row_non_string_deadline_shows_raw_value() {
        // Shape drift: a numeric `deadline` must render its raw value (first 10
        // chars), not crash and not vanish to '-'.
        let u = serde_json::json!({
            "priority": 100, "status": "testing", "kind": "Maintenance",
            "deadline": 12345, "id": "SUSE:Maintenance:1:2"
        });
        let row = render_row(&u, false);
        assert!(row.contains("12345"), "{row}");
        assert!(row.contains("SUSE:Maintenance:1:2"), "{row}");
    }

    #[tokio::test]
    async fn empty_queue_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Updates, &[]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("No updates in the queue"));
    }

    #[tokio::test]
    async fn fetch_failure_returns_err_not_empty_queue() {
        // A 5xx must propagate, distinct from a genuinely-empty queue.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Updates, &[]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        // Crucially it did NOT print the empty-queue message.
        assert!(!buf.contents().contains("No updates in the queue"));
    }

    /// A SMELT failure is TeReGen answering, not TeReGen being unreachable: the
    /// failure names the status and TeReGen's reason, and lists no rows.
    #[tokio::test]
    async fn smelt_failure_names_status_and_reason_not_unreachable() {
        for (status, reason, shown) in [
            (
                502,
                "SMELT update queue lookup failed",
                "TeReGen answered 502 Bad Gateway: SMELT update queue lookup failed",
            ),
            (
                504,
                "SMELT update queue lookup timed out",
                "TeReGen answered 504 Gateway Timeout: SMELT update queue lookup timed out",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/updates"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(serde_json::json!({"error": reason})),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (mut session, buf) = teregen_session(&server);
            let err = Updates
                .call(&mut session, &matches(&Updates, &[]))
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("Update queue query failed: {shown}")
            );
            assert!(buf.contents().is_empty(), "{}", buf.contents());
        }
    }

    /// A cancel while TeReGen is still deciding ends the wait as `Cancelled`.
    /// The outer timeout is the assertion: an unobserved cancel waits out the
    /// delay.
    #[tokio::test]
    async fn a_cancel_while_waiting_stops_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": []}))
                    .set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let cancel = session.cancel_token();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Updates.call(&mut session, &matches(&Updates, &[])),
        )
        .await
        .expect("the wait must observe the cancel")
        .unwrap_err();
        assert!(matches!(err, CommandError::Cancelled(_)), "{err:?}");
        assert!(buf.contents().is_empty(), "{}", buf.contents());
    }

    #[tokio::test]
    async fn missing_updates_key_is_empty_queue_not_err() {
        // No `updates` key is Ok(None): the empty-queue message, not an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"other": 1})))
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Updates, &[]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("No updates in the queue"));
    }

    // ------------------------------------------------------------ --mine

    /// The OBS API whose oscrc section `--mine` reads.
    const MINE_OBS_API: &str = "https://api.example.invalid";
    /// The oscrc `user` for [`MINE_OBS_API`].
    const PRINCIPAL: &str = "qamuser";
    /// The local session user, which `--mine` must never ask about.
    const LOCAL_USER: &str = "localuser";
    const GITEA_TOKEN: &str = "s3cr3t-gitea-token";

    /// Points `$OSC_CONFIG` at an oscrc holding `body`. Only valid inside
    /// `#[serial(osc_config_env)]`.
    fn oscrc_with(dir: &tempfile::TempDir, body: &str) -> crate::commands::testkit::NoOscrc {
        let oscrc = dir.path().join("oscrc");
        std::fs::write(&oscrc, body).unwrap();
        crate::commands::testkit::NoOscrc::set_to(&oscrc)
    }

    /// An oscrc naming [`PRINCIPAL`] for [`MINE_OBS_API`], with a signing key.
    fn principal_oscrc(dir: &tempfile::TempDir) -> crate::commands::testkit::NoOscrc {
        oscrc_with(
            dir,
            &format!("[{MINE_OBS_API}]\nuser = {PRINCIPAL}\nsshkey = SHA256:abc123\n"),
        )
    }

    /// A session whose TeReGen and Gitea are both `server`, with a Gitea token
    /// only when `gitea` is set.
    fn mine_session(
        server: &MockServer,
        gitea: bool,
    ) -> (Session, crate::commands::testkit::Buffer) {
        let (mut session, buf) = teregen_session(server);
        session.config.obs_api_url = MINE_OBS_API.to_owned();
        session.config.session_user = LOCAL_USER.to_owned();
        session.config.gitea_url = server.uri();
        if gitea {
            session.config.gitea_token = GITEA_TOKEN.to_owned();
        }
        (session, buf)
    }

    /// `GET /api/v1/user` answering `login` to [`GITEA_TOKEN`] alone, once.
    async fn mount_gitea_login(server: &MockServer, login: &str) {
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .and(header("Authorization", format!("token {GITEA_TOKEN}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"login": login})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    /// The queue for `assignee`, asked for `times` times.
    async fn mount_assigned(
        server: &MockServer,
        assignee: &str,
        rows: serde_json::Value,
        times: u64,
    ) {
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("assignee", assignee))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updates": rows
            })))
            .expect(times)
            .mount(server)
            .await;
    }

    fn assigned_row(id: &str, priority: i64, assignee: &str) -> serde_json::Value {
        serde_json::json!({
            "priority": priority, "status": "testing", "kind": "Maintenance",
            "id": id, "assignee": assignee
        })
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_uses_the_oscrc_principal_and_limits_rows() {
        let server = MockServer::start().await;
        let rows = serde_json::json!([
            assigned_row("a", 1, PRINCIPAL),
            assigned_row("b", 2, PRINCIPAL),
        ]);
        mount_assigned(&server, PRINCIPAL, rows, 1).await;
        mount_assigned(&server, LOCAL_USER, serde_json::json!([]), 0).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, false);

        let args = matches(&Updates, &["--mine", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains(&format!("assignee={PRINCIPAL}")), "{out}");
        assert!(!out.contains("note:"), "{out}");
    }

    /// Assignments are recorded under the oscrc `user` whether or not this
    /// machine can sign as it: no `sshkey` (a password or keyring oscrc) and
    /// a missing key file both still yield the login.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_reads_the_oscrc_user_without_a_signing_key() {
        for body in [
            format!("[{MINE_OBS_API}]\nuser = {PRINCIPAL}\n"),
            format!(
                "[{MINE_OBS_API}]\nuser = {PRINCIPAL}\n\
                 credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n"
            ),
            format!("[{MINE_OBS_API}]\nuser = {PRINCIPAL}\nsshkey = /nonexistent/id_missing\n"),
        ] {
            let server = MockServer::start().await;
            let rows = serde_json::json!([assigned_row("a", 1, PRINCIPAL)]);
            mount_assigned(&server, PRINCIPAL, rows, 1).await;
            let dir = tempfile::TempDir::new().unwrap();
            let _oscrc = oscrc_with(&dir, &body);
            let (mut session, buf) = mine_session(&server, false);

            let args = matches(&Updates, &["--mine"]);
            let result = Updates.call(&mut session, &args).await;
            assert!(result.is_ok(), "{body}: {result:?}");
            let out = buf.contents();
            assert!(out.contains("Update queue (1):"), "{body}: {out}");
            assert!(!out.contains("note:"), "{body}: {out}");
        }
    }

    /// Headlessly the refusal tells the three oscrc faults apart without
    /// naming the oscrc file.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_names_the_oscrc_fault_without_a_path() {
        let server = MockServer::start().await;
        let other = format!("[https://other.example.invalid]\nuser = {PRINCIPAL}\n");
        let no_user = format!("[{MINE_OBS_API}]\nsshkey = SHA256:abc123\n");
        for (body, fault) in [
            (None, "no oscrc file"),
            (Some(other), "no oscrc section for it"),
            (Some(no_user), "no user in its oscrc section"),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let _oscrc = match &body {
                Some(body) => oscrc_with(&dir, body),
                None => crate::commands::testkit::NoOscrc::set(),
            };
            let (mut session, _buf) = mine_session(&server, false);

            let args = matches(&Updates, &["--mine"]);
            let msg = Updates
                .call(&mut session, &args)
                .await
                .unwrap_err()
                .to_string();
            assert!(msg.contains(&format!(": {fault},")), "{msg}");
            assert!(msg.contains("the OBS login"), "{msg}");
            let tmp = dir.path().display().to_string();
            assert!(!msg.contains(&tmp), "headless leaks a path: {msg}");
            assert!(
                !msg.contains("/nonexistent"),
                "headless leaks a path: {msg}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_merges_the_gitea_login_rows_once_each_by_priority() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, "giteauser").await;
        let obs_rows = serde_json::json!([
            assigned_row("row-both", 5, PRINCIPAL),
            assigned_row("row-obs", 1, PRINCIPAL),
        ]);
        mount_assigned(&server, PRINCIPAL, obs_rows, 1).await;
        let gitea_rows = serde_json::json!([
            assigned_row("row-both", 5, PRINCIPAL),
            assigned_row("row-gitea", 3, "giteauser"),
        ]);
        mount_assigned(&server, "giteauser", gitea_rows, 1).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (3):"), "{out}");
        assert_eq!(out.matches("row-both").count(), 1, "{out}");
        let (both, gitea, obs) = (
            out.find("row-both").unwrap(),
            out.find("row-gitea").unwrap(),
            out.find("row-obs").unwrap(),
        );
        assert!(both < gitea && gitea < obs, "order wrong: {out}");
        assert!(!out.contains(GITEA_TOKEN), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_asks_once_when_the_gitea_login_is_the_principal() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, PRINCIPAL).await;
        let rows = serde_json::json!([assigned_row("a", 1, PRINCIPAL)]);
        mount_assigned(&server, PRINCIPAL, rows, 1).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        // wiremock verifies the single queue query on drop.
    }

    #[tokio::test]
    async fn empty_assignee_answer_names_the_login() {
        let server = MockServer::start().await;
        mount_assigned(&server, "bob", serde_json::json!([]), 1).await;
        let (mut session, buf) = teregen_session(&server);

        let args = matches(&Updates, &["--assignee", "bob"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert_eq!(
            buf.contents().trim_end(),
            "No updates assigned to bob in the queue"
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn empty_mine_answer_names_both_logins() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, "giteauser").await;
        mount_assigned(&server, PRINCIPAL, serde_json::json!([]), 1).await;
        mount_assigned(&server, "giteauser", serde_json::json!([]), 1).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert_eq!(
            buf.contents().trim_end(),
            format!("No updates assigned to {PRINCIPAL} or giteauser in the queue")
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn empty_mine_json_stays_an_empty_array() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, "giteauser").await;
        mount_assigned(&server, PRINCIPAL, serde_json::json!([]), 1).await;
        mount_assigned(&server, "giteauser", serde_json::json!([]), 1).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine", "--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert_eq!(buf.contents(), "[]\n");
    }

    /// Headless, the refusal names the OBS api but no local path, and no
    /// queue is asked for any stand-in login.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_refuses_an_unreadable_oscrc_without_a_gitea_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updates": []
            })))
            .expect(0)
            .mount(&server)
            .await;
        let _oscrc = crate::commands::testkit::NoOscrc::set();
        let (mut session, buf) = mine_session(&server, false);

        let args = matches(&Updates, &["--mine"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("the OBS login"), "{msg}");
        assert!(msg.contains(MINE_OBS_API), "{msg}");
        assert!(msg.contains("no oscrc file"), "{msg}");
        assert!(msg.contains("no [gitea] token"), "{msg}");
        assert!(msg.contains("--assignee"), "{msg}");
        assert!(
            !msg.contains("/nonexistent"),
            "headless leaks a path: {msg}"
        );
        assert!(!msg.contains(LOCAL_USER), "{msg}");
        assert_eq!(buf.contents(), "", "nothing listed");
    }

    /// At the REPL the oscrc reason (which names local files) is shown.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_at_the_repl_names_the_oscrc_reason() {
        let server = MockServer::start().await;
        let _oscrc = crate::commands::testkit::NoOscrc::set();
        let (mut session, _buf) = mine_session(&server, false);
        session.is_repl = true;

        let args = matches(&Updates, &["--mine"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/oscrc-for-tests"), "{msg}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_lists_the_gitea_login_alone_with_a_note_without_an_oscrc() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, "giteauser").await;
        let rows = serde_json::json!([assigned_row("row-gitea", 3, "giteauser")]);
        mount_assigned(&server, "giteauser", rows, 1).await;
        let _oscrc = crate::commands::testkit::NoOscrc::set();
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let note = out.lines().next().unwrap_or_default();
        assert!(note.starts_with("note: the OBS login "), "{out}");
        assert!(note.contains(MINE_OBS_API), "{out}");
        assert!(note.contains("could not be read: no oscrc file"), "{out}");
        assert!(note.contains("only updates assigned to giteauser"), "{out}");
        assert!(
            !note.contains("/nonexistent"),
            "headless leaks a path: {out}"
        );
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("row-gitea"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_notes_a_failing_gitea_lookup_and_lists_the_principal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        mount_assigned(&server, PRINCIPAL, serde_json::json!([]), 1).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let mut lines = out.lines();
        let note = lines.next().unwrap_or_default();
        assert!(note.starts_with("note: the Gitea login "), "{out}");
        assert!(note.contains("500"), "the reason is kept: {out}");
        assert!(
            note.contains(&format!("only updates assigned to {PRINCIPAL}")),
            "{out}"
        );
        assert_eq!(
            lines.next(),
            Some(format!("No updates assigned to {PRINCIPAL} in the queue").as_str()),
            "{out}"
        );
        assert!(!out.contains(GITEA_TOKEN), "{out}");
    }

    /// `--json` cannot carry a note, so a login it could not resolve fails
    /// the call instead of vanishing from a valid-looking array.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_json_refuses_a_partial_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        mount_assigned(&server, PRINCIPAL, serde_json::json!([]), 0).await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine", "--json"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Gitea"), "{msg}");
        assert!(msg.contains("--assignee"), "{msg}");
        assert_eq!(buf.contents(), "", "no JSON printed");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_fails_when_neither_login_resolves() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let _oscrc = crate::commands::testkit::NoOscrc::set();
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("oscrc"), "{msg}");
        assert!(msg.contains("Gitea"), "{msg}");
        assert!(!msg.contains(GITEA_TOKEN), "{msg}");
        assert_eq!(buf.contents(), "", "nothing listed");
    }

    // ------------------------------------------------ repeatable review-group

    /// Builds a session pointed at `server`.
    fn teregen_session(server: &MockServer) -> (Session, crate::commands::testkit::Buffer) {
        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;
        (session, buf)
    }

    /// The `review_group` values of every request `server` received, one entry
    /// per request.
    async fn requested_groups(server: &MockServer) -> Vec<Vec<String>> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                r.url
                    .query_pairs()
                    .filter(|(k, _)| k == "review_group")
                    .map(|(_, v)| v.into_owned())
                    .collect()
            })
            .collect()
    }

    /// Several `-G` are one query: TeReGen serves the union of a repeated
    /// `review_group`, so the rows come back merged and deduplicated.
    #[tokio::test]
    async fn several_groups_are_one_query_carrying_all_of_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 10, "status": "testing", "kind": "Maintenance", "id": "row-a"},
                    {"priority": 7, "status": "testing", "kind": "Maintenance", "id": "row-c"},
                    {"priority": 5, "status": "testing", "kind": "Maintenance", "id": "row-both"},
                ]})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &[
                "--review-group",
                "qam-sle",
                "--review-group",
                "qam-teradata-review",
            ],
        );
        Updates.call(&mut session, &args).await.unwrap();

        assert_eq!(
            requested_groups(&server).await,
            vec![vec!["qam-sle".to_owned(), "qam-teradata-review".to_owned()]]
        );
        let out = buf.contents();
        assert!(out.contains("Update queue (3):"), "{out}");
        assert!(
            out.find("row-a").unwrap() < out.find("row-c").unwrap(),
            "{out}"
        );
    }

    /// `--mine` asks once per login, and each query carries every group.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_with_several_groups_asks_once_per_login() {
        let server = MockServer::start().await;
        mount_gitea_login(&server, "giteauser").await;
        mount_assigned(
            &server,
            PRINCIPAL,
            serde_json::json!([assigned_row("a", 2, PRINCIPAL)]),
            1,
        )
        .await;
        mount_assigned(
            &server,
            "giteauser",
            serde_json::json!([assigned_row("b", 1, "giteauser")]),
            1,
        )
        .await;
        let dir = tempfile::TempDir::new().unwrap();
        let _oscrc = principal_oscrc(&dir);
        let (mut session, buf) = mine_session(&server, true);

        let args = matches(&Updates, &["--mine", "-G", "g1", "-G", "g2"]);
        Updates.call(&mut session, &args).await.unwrap();

        let updates_requests: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/updates")
            .collect();
        assert_eq!(updates_requests.len(), 2);
        for r in &updates_requests {
            let groups: Vec<_> = r
                .url
                .query_pairs()
                .filter(|(k, _)| k == "review_group")
                .map(|(_, v)| v.into_owned())
                .collect();
            assert_eq!(groups, ["g1", "g2"], "{}", r.url);
        }
        assert!(
            buf.contents().contains("Update queue (2):"),
            "{}",
            buf.contents()
        );
    }

    /// One failing query lists nothing: no partial listing is printed.
    #[tokio::test]
    async fn a_failing_multi_group_query_is_an_error_not_partial_output() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(502))
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &["--review-group", "good", "--review-group", "bad"],
        );
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        assert!(buf.contents().is_empty(), "{}", buf.contents());
    }

    #[tokio::test]
    async fn limit_slices_the_one_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 100, "status": "testing", "kind": "Maintenance", "id": "high"},
                    {"priority": 1, "status": "testing", "kind": "Maintenance", "id": "low"},
                ]})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-G", "g1", "-G", "g2", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("high"), "{out}");
        assert!(!out.contains("low"), "{out}");
    }

    // ------------------------------------------------------- field selection

    /// A distinct value in every servable field, so a swapped extractor
    /// mapping cannot pass.
    fn distinct_row() -> serde_json::Value {
        serde_json::json!({
            "id": "SUSE:Maintenance:77:707",
            "priority": 421,
            "rating": "important",
            "category": "security",
            "status": "testing",
            "kind": "Maintenance",
            "deadline": "2026-08-09T10:00:00Z",
            "assignee": "alice",
            "assignees": {"qam-sle": [{"state": "assigned", "user": "alice"}]},
            "review_groups": ["qam-sle", "qam-openqa", "qam-manager"],
            "title": "Security update for demo",
            "url": "/request/707/",
        })
    }

    #[tokio::test]
    async fn field_selection_renders_each_field_under_its_canonical_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [distinct_row()]})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        // Canonical, raw-key alias, separator/case variants must all resolve.
        let args = matches(
            &Updates,
            &[
                "-F",
                "id",
                "-F",
                "incident-priority",
                "-F",
                "Rating",
                "-F",
                "CATEGORY",
                "-F",
                "Assigned Roles",
                "-F",
                "unassigned_roles",
                "-F",
                "Title",
                "-F",
                "URL",
                "-F",
                "Deadline",
                "-F",
                "Assignee",
                "-F",
                "Status",
                "-F",
                "Kind",
            ],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // A swapped mapping would pair a value with the wrong name.
        for line in [
            "ReviewRequestID: SUSE:Maintenance:77:707",
            "Incident Priority: 421",
            "Rating: important",
            "Category: security",
            "Assigned Roles: qam-sle:alice",
            // review_groups order is the server's, preserved verbatim.
            "Unassigned Roles: qam-openqa, qam-manager",
            "Title: Security update for demo",
            "URL: /request/707/",
            "Deadline: 2026-08-09T10:00:00Z",
            "Assignee: alice",
            "Status: testing",
            "Kind: Maintenance",
        ] {
            assert!(out.contains(line), "missing '{line}' in:\n{out}");
        }
    }

    #[tokio::test]
    async fn unassigned_roles_on_slfo_row_is_na_not_empty() {
        // A row without review_groups has no role data; the honest render is n/a.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 9, "status": "testing", "kind": "SLFO",
                     "id": "SUSE:SLFO:1.2:9",
                     "assignees": {"qam-sle": [{"state": "assigned", "user": "bob"}]}},
                ]})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &["-F", "Unassigned Roles", "-F", "Assigned Roles"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Unassigned Roles: n/a"), "{out}");
        assert!(out.contains("Assigned Roles: qam-sle:bob"), "{out}");
    }

    #[tokio::test]
    async fn non_assigned_review_state_stays_visible() {
        let obj = serde_json::json!({
            "assignees": {"qam-sle": [{"state": "review", "user": "carol"}]}
        });
        let rendered = extract_assigned_roles(obj.as_object().unwrap());
        assert_eq!(rendered, "qam-sle:carol(review)");
    }

    #[tokio::test]
    async fn unknown_field_errors_before_any_query() {
        // The expect(0) mock pins "before any query" independently of the
        // error text: a resolve-after-fetch refactor trips it.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let (mut session, _buf) = teregen_session(&server);
        let args = matches(&Updates, &["-F", "Bogus"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field 'Bogus'"), "{msg}");
        assert!(msg.contains("ReviewRequestID"), "{msg}");
    }

    #[tokio::test]
    async fn subset_selection_renders_only_requested_fields_in_request_order() {
        // Kills the mutation of iterating the full FIELDS registry instead of
        // the requested specs, which passes the all-12 test.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [distinct_row()]})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        // Deliberately not in FIELDS declaration order: Status before Rating.
        let args = matches(&Updates, &["-F", "Status", "-F", "Rating"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Status: testing"), "{out}");
        assert!(out.contains("Rating: important"), "{out}");
        // Unselected fields must be absent.
        for absent in ["ReviewRequestID:", "Category:", "Title:", "URL:"] {
            assert!(!out.contains(absent), "'{absent}' leaked into:\n{out}");
        }
        // Order follows the request, not the registry.
        assert!(
            out.find("Status:").unwrap() < out.find("Rating:").unwrap(),
            "requested order not honoured: {out}"
        );
    }

    /// The query string of the one request `server` received.
    async fn only_query(server: &MockServer) -> String {
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        requests[0].url.query().unwrap_or_default().to_owned()
    }

    async fn serving_empty_queue() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .mount(&server)
            .await;
        server
    }

    /// `--status tested` is the plain tested queue. Sending the testing
    /// queue's assignment params would make TeReGen answer testing instead.
    #[tokio::test]
    async fn a_status_other_than_testing_sends_no_assignment_params() {
        for status in ["tested", "patchinfo_missing"] {
            let server = serving_empty_queue().await;
            let (mut session, _buf) = teregen_session(&server);
            let args = matches(&Updates, &["--status", status]);
            Updates.call(&mut session, &args).await.unwrap();
            assert_eq!(only_query(&server).await, format!("status={status}"));
        }
    }

    /// `--status all` sends no status and no assignment params either.
    #[tokio::test]
    async fn status_all_sends_no_assignment_params() {
        let server = serving_empty_queue().await;
        let (mut session, _buf) = teregen_session(&server);
        Updates
            .call(&mut session, &matches(&Updates, &["--status", "all"]))
            .await
            .unwrap();
        assert_eq!(only_query(&server).await, "");
    }

    /// An assignment view asks for testing updates; with another status it
    /// would be answered with the wrong queue, so it is refused unasked.
    #[tokio::test]
    async fn an_assignment_view_with_another_status_is_refused_before_any_request() {
        for flags in [
            &["--status", "tested", "--mine"][..],
            &["--status", "all", "--assignee", "alice"],
            &["--status", "all", "--all-assignees"],
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let (mut session, buf) = teregen_session(&server);
            let err = Updates
                .call(&mut session, &matches(&Updates, flags))
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("apply to --status testing only"),
                "{flags:?}: {msg}"
            );
            assert!(buf.contents().is_empty(), "{}", buf.contents());
        }
    }

    /// The default view is unchanged: unassigned updates in testing.
    #[tokio::test]
    async fn the_default_view_is_the_unassigned_testing_queue() {
        let server = serving_empty_queue().await;
        let (mut session, _buf) = teregen_session(&server);
        Updates
            .call(&mut session, &matches(&Updates, &[]))
            .await
            .unwrap();
        assert_eq!(
            only_query(&server).await,
            "status=testing&unassigned=1&with_assignment=1"
        );
    }

    #[tokio::test]
    async fn assignment_fields_with_status_all_are_refused() {
        // Printing "unassigned" for an SLFO row TeReGen served without
        // assignment data would be false. expect(0) pins the pre-query refusal.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let (mut session, _buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all", "-F", "Assigned Roles"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'Assigned Roles' needs assignment data"),
            "{msg}"
        );
        assert!(msg.contains("--status testing"), "{msg}");

        // The same field in an assignment view is fine (--all-assignees).
        let server2 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("with_assignment", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [distinct_row()]})),
            )
            .mount(&server2)
            .await;
        let (mut session2, buf2) = teregen_session(&server2);
        let args2 = matches(&Updates, &["--all-assignees", "-F", "Assigned Roles"]);
        Updates.call(&mut session2, &args2).await.unwrap();
        assert!(
            buf2.contents().contains("Assigned Roles: qam-sle:alice"),
            "{}",
            buf2.contents()
        );
        // Non-assignment fields under --status all stay allowed.
        let server3 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [distinct_row()]})),
            )
            .mount(&server3)
            .await;
        let (mut session3, buf3) = teregen_session(&server3);
        let args3 = matches(&Updates, &["--status", "all", "-F", "Rating"]);
        Updates.call(&mut session3, &args3).await.unwrap();
        assert!(buf3.contents().contains("Rating: important"));
    }

    #[tokio::test]
    async fn duplicate_group_input_collapses_to_one_query() {
        // `-G a -G a` fires one query, not two identical ones.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "qam-sle"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 3, "status": "testing", "kind": "Maintenance", "id": "solo"},
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-G", "qam-sle", "-G", "qam-sle"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("Update queue (1):"));
    }

    #[tokio::test]
    async fn malformed_updates_value_errors_on_both_paths() {
        // `{"updates": 5}` (not an array) must error, not read as empty.
        for extra_group in [None, Some(["-G", "a", "-G", "b"])] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/updates"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": 5})),
                )
                .mount(&server)
                .await;
            let (mut session, buf) = teregen_session(&server);
            let argv: Vec<&str> = extra_group.into_iter().flatten().collect();
            let args = matches(&Updates, &argv);
            let err = Updates.call(&mut session, &args).await.unwrap_err();
            assert!(
                err.to_string().contains("malformed response"),
                "path {argv:?}: {err}"
            );
            assert!(!buf.contents().contains("No updates in the queue"));
        }
    }

    #[tokio::test]
    async fn merge_keeps_idless_rows_and_skips_missing_updates_key() {
        // Two branches: a body with no `updates` key (skipped, not fatal) and
        // a row without an `id` (kept, not silently dropped).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "has-rows"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 9, "status": "testing", "kind": "Maintenance", "id": "with-id"},
                    {"priority": 8, "status": "testing", "kind": "Maintenance"},
                ]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "no-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"other": 1})))
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-G", "has-rows", "-G", "no-key"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // Both rows survive: the id-less one is not dropped.
        assert!(out.contains("Update queue (2):"), "{out}");
        assert!(out.contains("with-id"), "{out}");
    }

    #[test]
    fn non_object_rows_render_without_panicking() {
        let scalar = serde_json::json!("stray-string");
        assert_eq!(render_row(&scalar, true), "  \"stray-string\"");
        let spec = resolve_field("Rating").unwrap();
        assert_eq!(render_fields(&scalar, &[spec]), "  \"stray-string\"");
    }

    #[tokio::test]
    async fn obs_backed_fields_resolve_and_list_as_available() {
        // Kills keeping Package-Streams/Issues/Comments in the refused set
        // now that the OBS fallback serves them.
        for name in [
            "Package-Streams",
            "package_streams",
            "Issues",
            "ISSUES",
            "Comments",
            "comments",
        ] {
            resolve_field(name).unwrap();
        }
        for name in ["Package-Streams", "Issues", "Comments"] {
            assert!(available_field_names().contains(name), "{name}");
        }
    }

    #[tokio::test]
    async fn package_streams_never_aliases_codestreams() {
        // Live rows carry `codestreams` (project identifiers), but osc-qam's
        // `Package-Streams` is the OBS `src_package` set (bare names): the
        // former must not satisfy the latter, with or without the fallback.
        // Kills reading `codestreams` as `Package-Streams`, and a
        // `codestreams` alias resolving at all.
        let row = serde_json::json!({
            "id": "SUSE:Maintenance:1:1",
            "codestreams": ["SUSE:SLE-15-SP2:Update"],
        });
        let o = row.as_object().unwrap();
        assert_eq!(
            string_list_field(o, "codestreams"),
            "SUSE:SLE-15-SP2:Update"
        );
        let spec = resolve_field("Package-Streams").unwrap();
        assert_eq!(render_fields(&row, &[spec]), "  Package-Streams: -");
        let msg = resolve_field("codestreams").err().unwrap().to_string();
        assert!(msg.contains("unknown field 'codestreams'"), "{msg}");
    }

    // ------------------------------------------- OBS fallback (#415)

    const OBS_REQUEST_XML: &str = "<request id='56789'>\
        <action type='maintenance_incident'>\
        <source project='SUSE:Maintenance:1' package='glibc'/>\
        <source project='SUSE:Maintenance:1' package='openssl'/></action>\
        <issue tracker='bnc' name='1'/>\
        <state name='review'/></request>";
    const OBS_COMMENTS_XML: &str = "<comments>\
        <comment who='carol' when='2024-01-01T00:00:00'>ship it</comment></comments>";

    /// A session whose TeReGen queue and OBS API are two mocks, with oscrc
    /// resolving against the OBS mock. Only valid inside
    /// `#[serial(osc_config_env)]`: holds the `$OSC_CONFIG` guard. Both
    /// servers are returned so the caller keeps them alive past the call.
    async fn obs_backed_session(
        queue: serde_json::Value,
    ) -> (
        Session,
        crate::commands::testkit::Buffer,
        MockServer,
        MockServer,
        crate::commands::testkit::NoOscrc,
        tempfile::TempDir,
    ) {
        use crate::commands::testkit::NoOscrc;
        use crate::commands::testkit::teregen::write_oscrc;
        let teregen = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(queue))
            .mount(&teregen)
            .await;
        let obs = MockServer::start().await;
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = write_oscrc(dir.path(), &obs);
        let guard = NoOscrc::set_to(&oscrc);
        let (mut session, buf) = empty_session();
        session.config.teregen_api = teregen.uri();
        session.config.obs_api_url = obs.uri();
        (session, buf, teregen, obs, guard, dir)
    }

    async fn mount_obs_row(server: &MockServer, review_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/request/{review_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(OBS_REQUEST_XML))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/comments/request/{review_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(OBS_COMMENTS_XML))
            .mount(server)
            .await;
    }

    fn queue_row(review_id: u64) -> serde_json::Value {
        serde_json::json!({
            "priority": 1, "status": "testing", "kind": "Maintenance",
            "id": format!("SUSE:Maintenance:1:{review_id}"),
        })
    }

    async fn obs_gets(server: &MockServer, url_path: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == url_path)
            .count()
    }

    #[test]
    fn inject_extras_ignores_non_object_rows() {
        // Kills an enrichment panic on scalar rows: without an object there
        // is nothing to inject into, so the row passes through untouched.
        let mut row = serde_json::json!("stray-string");
        inject_extras(
            &mut row,
            &qam::RequestExtras {
                src_packages: vec!["glibc".to_owned()],
                issues: vec![],
                comments: vec![],
                comments_unavailable: false,
            },
        );
        assert_eq!(row, serde_json::json!("stray-string"));
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_backed_fields_on_idless_rows_render_dashes_without_fetch() {
        // Kills fetching (or failing on) rows with no id: there is nothing to
        // look up, so the OBS mock must see zero calls and the row renders
        // dashes. The skip is debug-logged so a future id-shape break is
        // diagnosable rather than silent.
        use crate::commands::testkit::log_capture;
        log_capture::start();
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [
                {"priority": 1, "status": "testing", "kind": "Maintenance"},
            ]}))
            .await;
        let args = matches(&Updates, &["-F", "Package-Streams"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: -"), "{out}");
        assert!(obs.received_requests().await.unwrap().is_empty());
        let logged = log_capture::take();
        assert!(
            logged.iter().any(|c| c
                .message
                .as_deref()
                .is_some_and(|m| m.contains("no shown row has a parseable RequestReviewID"))),
            "expected the enrichment-skip debug log, got {:?}",
            logged
                .iter()
                .filter_map(|c| c.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_backed_fields_served_from_obs_fallback() {
        // Kills serving TeReGen-shaped placeholders (or dashes) for the OBS
        // fields: every value below comes from the OBS mocks, not the queue.
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [queue_row(56789)]})).await;
        mount_obs_row(&obs, "56789").await;
        let args = matches(
            &Updates,
            &["-F", "Package-Streams", "-F", "Issues", "-F", "Comments"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert!(out.contains("Issues: bnc#1"), "{out}");
        assert!(
            out.contains("Comments: carol (2024-01-01T00:00:00): ship it"),
            "{out}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn slfo_rows_render_na_without_obs_read() {
        // Kills serving another request's data on non-OBS rows: the SLFO
        // row's review id is a Gitea PR number, so OBS must see no read for
        // it and the OBS-backed fields render n/a, while the Maintenance
        // neighbour is still served from OBS.
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [
                queue_row(56789),
                {"priority": 9, "status": "testing", "kind": "SLFO",
                 "id": "SUSE:SLFO:1.2:9"},
            ]}))
            .await;
        mount_obs_row(&obs, "56789").await;
        let args = matches(
            &Updates,
            &["-F", "Package-Streams", "-F", "Issues", "-F", "Comments"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert!(out.contains("Package-Streams: n/a"), "{out}");
        assert!(out.contains("Issues: n/a"), "{out}");
        assert!(out.contains("Comments: n/a"), "{out}");
        assert_eq!(obs_gets(&obs, "/request/9").await, 0);
        assert_eq!(obs_gets(&obs, "/comments/request/9").await, 0);
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_fallback_not_hit_when_unrequested() {
        // Kills an unconditional OBS fan-out: TeReGen-only fields must not
        // touch OBS at all (the .expect(0) trips on any GET). Pinned against
        // a missing oscrc so the kill is deterministic, not ambient-dependent.
        use crate::commands::testkit::NoOscrc;
        let _guard = NoOscrc::set();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [queue_row(56789)]})),
            )
            .mount(&server)
            .await;
        let obs = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
            .expect(0)
            .mount(&obs)
            .await;
        let (mut session, buf) = teregen_session(&server);
        session.config.obs_api_url = obs.uri();
        let args = matches(&Updates, &["-F", "Rating"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("Rating:"), "{}", buf.contents());
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_fallback_runs_only_for_shown_rows() {
        // Kills fetching before --limit/--offset: a 3-row queue under
        // --limit 1 costs exactly one request GET + one comments GET.
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [
                queue_row(101), queue_row(102), queue_row(103),
            ]}))
            .await;
        mount_obs_row(&obs, "101").await;
        let args = matches(&Updates, &["-F", "Package-Streams", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert_eq!(obs_gets(&obs, "/request/101").await, 1);
        assert_eq!(obs_gets(&obs, "/comments/request/101").await, 1);
        assert_eq!(obs_gets(&obs, "/request/102").await, 0);
        assert_eq!(obs_gets(&obs, "/request/103").await, 0);
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn partial_obs_failure_degrades_only_its_row() {
        // Kills failing the whole listing over one stale row: the row OBS
        // 404s marks `unavailable` while its neighbour serves.
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [
                queue_row(101), queue_row(102),
            ]}))
            .await;
        mount_obs_row(&obs, "101").await;
        Mock::given(method("GET"))
            .and(path("/request/102"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&obs)
            .await;
        let args = matches(&Updates, &["-F", "Package-Streams"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert!(out.contains("Package-Streams: unavailable"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn failed_obs_row_marks_all_three_fields_unavailable() {
        // Kills rendering a broken OBS read as "nothing there": the row OBS
        // 404s marks every OBS-backed field `unavailable`, distinct from `-`
        // (looked, found nothing) and `n/a` (SLFO, never looked up).
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [
                queue_row(101), queue_row(102),
            ]}))
            .await;
        mount_obs_row(&obs, "101").await;
        Mock::given(method("GET"))
            .and(path("/request/102"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&obs)
            .await;
        let args = matches(
            &Updates,
            &["-F", "Package-Streams", "-F", "Issues", "-F", "Comments"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert!(out.contains("Package-Streams: unavailable"), "{out}");
        assert!(out.contains("Issues: unavailable"), "{out}");
        assert!(out.contains("Comments: unavailable"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn unreadable_comments_marks_only_comments_unavailable() {
        // Kills rendering a broken comments document as "no comments": the
        // packages and issues from the served request document still print
        // while Comments marks `unavailable`, distinct from `-` (no comments).
        let (mut session, buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [queue_row(101)]})).await;
        Mock::given(method("GET"))
            .and(path("/request/101"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OBS_REQUEST_XML))
            .mount(&obs)
            .await;
        Mock::given(method("GET"))
            .and(path("/comments/request/101"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&obs)
            .await;
        let args = matches(
            &Updates,
            &["-F", "Package-Streams", "-F", "Issues", "-F", "Comments"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Package-Streams: glibc, openssl"), "{out}");
        assert!(out.contains("Issues: bnc#1"), "{out}");
        assert!(out.contains("Comments: unavailable"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_outage_for_every_row_is_an_error_not_silent_dashes() {
        // Kills silent all-dash degradation hiding a dead OBS base (a wrong
        // [obs] api_url, an unreadable oscrc): with no row servable the
        // command errors naming OBS instead of printing dashes.
        let (mut session, _buf, _teregen, obs, _guard, _dir) =
            obs_backed_session(serde_json::json!({"updates": [queue_row(101)]})).await;
        Mock::given(method("GET"))
            .and(path("/request/101"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&obs)
            .await;
        let args = matches(&Updates, &["-F", "Issues"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        assert!(
            err.to_string().contains("OBS details unavailable"),
            "{}",
            err.to_string()
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_backed_fields_without_oscrc_fail_before_any_obs_read() {
        // Without credentials the OBS client build refuses once the queue is
        // known non-empty: the TeReGen query runs (expect(1)), but no OBS
        // read happens and the error names the credentials.
        use crate::commands::testkit::NoOscrc;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [queue_row(56789)]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _guard = NoOscrc::set();
        let (mut session, _buf) = teregen_session(&server);
        let args = matches(&Updates, &["-F", "Package-Streams"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        assert!(
            err.to_string().contains("could not build OBS client"),
            "{}",
            err.to_string()
        );
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_backed_fields_on_empty_queue_need_no_oscrc() {
        // Kills requiring oscrc when no OBS read would happen: an empty queue
        // prints the empty message without touching credentials.
        use crate::commands::testkit::NoOscrc;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .mount(&server)
            .await;
        let _guard = NoOscrc::set();
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-F", "Package-Streams"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("No updates in the queue"));
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_backed_fields_past_end_offset_needs_no_oscrc() {
        // Kills building the OBS client before windowing: an offset past the
        // last row shows an empty listing without touching credentials.
        use crate::commands::testkit::NoOscrc;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": [queue_row(56789)]})),
            )
            .mount(&server)
            .await;
        let _guard = NoOscrc::set();
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-F", "Issues", "--offset", "99999"]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("Update queue (0):"));
    }

    #[test]
    fn obs_comment_format_and_extraction_edges() {
        // Kills dropping who/when from the line, leaking a comment's own
        // newlines into the block, joining comments with ", ", rendering a
        // missing/empty list as anything but "-", emitting an unindented
        // continuation line, and leaving a dangling separator on an empty
        // body.
        let line = |who: &str, when: &str, body: &str| {
            format_obs_comment(&ObsComment {
                who: who.to_owned(),
                when: when.to_owned(),
                body: body.to_owned(),
            })
        };
        assert_eq!(line("a", "t", "x"), "a (t): x");
        assert_eq!(line("a", "", "x"), "a: x");
        assert_eq!(line("", "t", "x"), "(t): x");
        assert_eq!(line("", "", "  multi\n line "), "multi line");
        assert_eq!(line("a", "t", ""), "a (t)");
        assert_eq!(line("a", "", ""), "a");
        assert_eq!(line("", "", ""), "");
        let row = serde_json::json!({"obs_comments": ["a (t): x", "b: y"]});
        assert_eq!(
            extract_obs_comments(row.as_object().unwrap()),
            "a (t): x\n  b: y"
        );
        for row in [
            serde_json::json!({"obs_comments": []}),
            serde_json::json!({"obs_comments": [""]}),
            serde_json::json!({}),
        ] {
            assert_eq!(extract_obs_comments(row.as_object().unwrap()), "-");
        }
    }

    #[test]
    fn obs_comments_drop_terminal_sequences_and_bidi_overrides() {
        // Kills printing least-trusted text raw: an OBS comment body (or
        // who/when) carrying CSI/OSC escapes and bidi overrides must not
        // reach the terminal — or the MCP transcript — verbatim.
        let line = format_obs_comment(&ObsComment {
            who: "mallory\x1b]0;pwned\x07".to_owned(),
            when: "2024-01-01T00:00:00".to_owned(),
            body: "\x1b[2J\u{202B}rm -rf ~\x1b]52;c;eHlzdGVtCg==\x07".to_owned(),
        });
        assert!(!line.contains('\x1b'), "{line:?}");
        assert!(!line.contains('\u{7}'), "{line:?}");
        assert!(!line.contains('\u{202B}'), "{line:?}");
        // The OSC payload goes with its introducer, the way the terminal
        // itself would consume it — only the surrounding text survives.
        assert!(line.starts_with("mallory ("), "{line:?}");
        assert!(line.contains("rm -rf ~"), "{line:?}");
    }

    #[test]
    fn obs_comment_parts_cut_at_per_comment_limit() {
        // Kills an unbounded row in bytes: each comment part is cut to the
        // per-comment limit with the truncation marked, like checkers rows.
        let line = format_obs_comment(&ObsComment {
            who: "a".to_owned(),
            when: "t".to_owned(),
            body: "x".repeat(OBS_COMMENT_LIMIT + 50),
        });
        assert!(
            !line.contains(&"x".repeat(OBS_COMMENT_LIMIT + 1)),
            "{line:?}"
        );
        assert!(line.contains(TRUNCATION_MARK), "{line:?}");
    }

    #[test]
    fn obs_list_fields_strip_terminal_sequences_and_bidi_overrides() {
        // Kills printing OBS package/issue text raw: src_package names and
        // issue refs are OBS text like comments, so a CSI/OSC escape or bidi
        // override in either must not reach the terminal — or the MCP
        // transcript — verbatim.
        let row = serde_json::json!({
            "obs_package_streams": ["glibc\x1b[2J", "ok\u{202B}pkg"],
            "obs_issues": ["bnc#1\x1b]0;pwned\x07", "jsc#SLE-9"],
        });
        let o = row.as_object().unwrap();
        let streams = obs_string_list_field(o, "obs_package_streams");
        let issues = obs_string_list_field(o, "obs_issues");
        for rendered in [&streams, &issues] {
            assert!(!rendered.contains('\x1b'), "{rendered:?}");
            assert!(!rendered.contains('\u{7}'), "{rendered:?}");
            assert!(!rendered.contains('\u{202B}'), "{rendered:?}");
        }
        assert!(streams.contains("glibc"), "{streams:?}");
        assert!(streams.contains("okpkg"), "{streams:?}");
        assert!(issues.contains("bnc#1"), "{issues:?}");
        assert!(issues.contains("jsc#SLE-9"), "{issues:?}");
        // An element that is only an escape sequence leaves nothing to join.
        let row = serde_json::json!({"obs_issues": ["\x1b[2J"]});
        assert_eq!(
            obs_string_list_field(row.as_object().unwrap(), "obs_issues"),
            "-"
        );
    }

    #[test]
    fn inject_extras_keeps_newest_comments_by_when_not_document_order() {
        // Kills trusting OBS document order for "newest five": the kept tail
        // is the chronologically newest by `when`, so a document arriving
        // newest-first still renders the live discussion, not the oldest
        // chatter. `when` is ISO-8601 from OBS, so string order is
        // chronological; equal timestamps keep document order (stable).
        let extras = qam::RequestExtras {
            src_packages: Vec::new(),
            issues: Vec::new(),
            comments: (1..=7u32)
                .rev()
                .map(|i| ObsComment {
                    who: "u".to_owned(),
                    when: format!("2024-01-{i:02}T00:00:00"),
                    body: format!("c{i}"),
                })
                .collect(),
            comments_unavailable: false,
        };
        let mut row = serde_json::json!({});
        inject_extras(&mut row, &extras);
        let rendered = extract_obs_comments(row.as_object().unwrap());
        assert!(
            rendered.contains("2 earlier comments omitted"),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("u (2024-01-07T00:00:00): c7"),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("u (2024-01-06T00:00:00): c6"),
            "{rendered:?}"
        );
        assert!(
            !rendered.contains("u (2024-01-01T00:00:00): c1"),
            "{rendered:?}"
        );
        assert!(
            !rendered.contains("u (2024-01-02T00:00:00): c2"),
            "{rendered:?}"
        );
    }

    #[test]
    fn obs_partition_targets_only_obs_rows_and_stamps_non_obs() {
        // Kills looking a Gitea PR number up as an OBS request: SLFO review
        // ids are PR numbers (architecture.md), so only Maintenance and PI
        // rows are OBS-addressable. Unparseable ids stay skipped as before.
        // Non-OBS rows are stamped null in the same pass, so their OBS
        // fields render n/a.
        let row = |id: &str| serde_json::json!({"id": id});
        let mut rows = [
            row("SUSE:SLFO:1.2:9"),
            row("SUSE:Maintenance:1:56789"),
            row("SUSE:PI:42:7"),
            row("not-an-rrid"),
        ];
        let (targets, non_obs) = partition_obs_rows(&mut rows);
        let ids: Vec<u64> = targets.iter().map(|(_, r)| r.review_id).collect();
        assert_eq!(ids, [56789, 7]);
        assert_eq!(non_obs, [0]);
        assert!(
            rows[0]
                .get("obs_package_streams")
                .is_some_and(Value::is_null)
        );
        assert!(rows[0].get("obs_issues").is_some_and(Value::is_null));
        assert!(rows[0].get("obs_comments").is_some_and(Value::is_null));
        assert!(rows[1].get("obs_package_streams").is_none());
    }

    #[test]
    fn obs_unavailable_sentinel_renders_unavailable_not_dash() {
        // Kills rendering a broken OBS read as "nothing there": the
        // `unavailable` sentinel renders distinctly from `-` (looked, found
        // nothing) and `n/a` (SLFO, never looked up).
        let row = serde_json::json!({
            "obs_package_streams": "unavailable",
            "obs_issues": "unavailable",
            "obs_comments": "unavailable",
        });
        let o = row.as_object().unwrap();
        assert_eq!(
            obs_string_list_field(o, "obs_package_streams"),
            "unavailable"
        );
        assert_eq!(obs_string_list_field(o, "obs_issues"), "unavailable");
        assert_eq!(extract_obs_comments(o), "unavailable");
        // The neighbours keep their meanings: null is n/a, missing is -.
        let row = serde_json::json!({
            "obs_package_streams": null,
            "obs_issues": null,
            "obs_comments": null,
        });
        let o = row.as_object().unwrap();
        assert_eq!(obs_string_list_field(o, "obs_package_streams"), "n/a");
        assert_eq!(extract_obs_comments(o), "n/a");
        let row = serde_json::json!({});
        let o = row.as_object().unwrap();
        assert_eq!(obs_string_list_field(o, "obs_package_streams"), "-");
        assert_eq!(extract_obs_comments(o), "-");
    }

    #[test]
    fn inject_extras_marks_unreadable_comments_unavailable() {
        // Kills rendering a broken comments document as "no comments": the
        // packages and issues from the served request document still print
        // while Comments marks `unavailable`.
        let extras = qam::RequestExtras {
            src_packages: vec!["glibc".to_owned()],
            issues: vec!["bnc#1".to_owned()],
            comments: Vec::new(),
            comments_unavailable: true,
        };
        let mut row = serde_json::json!({});
        inject_extras(&mut row, &extras);
        let o = row.as_object().unwrap();
        assert_eq!(obs_string_list_field(o, "obs_package_streams"), "glibc");
        assert_eq!(obs_string_list_field(o, "obs_issues"), "bnc#1");
        assert_eq!(extract_obs_comments(o), "unavailable");
    }

    #[test]
    fn mark_obs_unavailable_stamps_all_three_fields() {
        // Kills a partial stamp leaving one field to render `-`: a refused
        // row marks every OBS-backed key.
        let mut row = serde_json::json!({"id": "SUSE:Maintenance:1:1"});
        mark_obs_unavailable(&mut row);
        let o = row.as_object().unwrap();
        assert_eq!(
            obs_string_list_field(o, "obs_package_streams"),
            "unavailable"
        );
        assert_eq!(obs_string_list_field(o, "obs_issues"), "unavailable");
        assert_eq!(extract_obs_comments(o), "unavailable");
        // A shapeless row is left alone rather than stamped.
        let mut stray = serde_json::json!("stray-string");
        mark_obs_unavailable(&mut stray);
        assert_eq!(stray, serde_json::json!("stray-string"));
    }

    #[test]
    fn inject_extras_keeps_last_comments_with_omitted_marker() {
        // Kills unbounded comment volume per row: beyond the cap only the
        // newest comments print, headed by a marker counting the omitted
        // ones; at exactly the cap there is no marker.
        let extras_for = |n: u64| qam::RequestExtras {
            src_packages: Vec::new(),
            issues: Vec::new(),
            comments: (1..=n)
                .map(|i| ObsComment {
                    who: "u".to_owned(),
                    when: "t".to_owned(),
                    body: format!("c{i}"),
                })
                .collect(),
            comments_unavailable: false,
        };
        let mut row = serde_json::json!({});
        inject_extras(&mut row, &extras_for(7));
        let rendered = extract_obs_comments(row.as_object().unwrap());
        assert!(
            rendered.contains("2 earlier comments omitted"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("u (t): c1"), "{rendered:?}");
        assert!(!rendered.contains("u (t): c2"), "{rendered:?}");
        assert!(rendered.contains("u (t): c7"), "{rendered:?}");
        let mut row = serde_json::json!({});
        inject_extras(&mut row, &extras_for(5));
        let rendered = extract_obs_comments(row.as_object().unwrap());
        assert!(!rendered.contains("omitted"), "{rendered:?}");
        assert!(rendered.contains("u (t): c1"), "{rendered:?}");
        // The singular marker reads "1 earlier comment omitted".
        let mut row = serde_json::json!({});
        inject_extras(&mut row, &extras_for(6));
        let rendered = extract_obs_comments(row.as_object().unwrap());
        assert!(
            rendered.contains("…[1 earlier comment omitted]"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("u (t): c1"), "{rendered:?}");
    }

    #[test]
    fn obs_extractors_ignore_teregen_shaped_keys() {
        // Kills reading a TeReGen-supplied `package_streams`/`issues`/
        // `comments` key as OBS data: the OBS extractors read only the
        // obs_-injected keys, so a future TeReGen row carrying those names
        // can neither satisfy the fields nor be silently overwritten.
        let row = serde_json::json!({
            "id": "SUSE:Maintenance:1:1",
            "package_streams": ["stray-pkg"],
            "issues": ["bnc#9"],
            "comments": ["stray"],
        });
        for (name, line) in [
            ("Package-Streams", "  Package-Streams: -"),
            ("Issues", "  Issues: -"),
            ("Comments", "  Comments: -"),
        ] {
            let spec = resolve_field(name).unwrap();
            assert_eq!(render_fields(&row, &[spec]), line);
        }
    }

    #[test]
    fn queue_list_fields_render_comma_joined_and_creator_as_is() {
        let row = serde_json::json!({
            "products": ["SLE-Product-SLES_15-SP6-LTSS", "SLE-Product-HPC_15-SP5-LTSS"],
            "srcrpms": ["dovecot23"],
            "bugs": ["bnc#1276794", "CVE-2026-27859"],
            "creator": "amattiazzo",
        });
        let o = row.as_object().unwrap();
        assert_eq!(
            string_list_field(o, "products"),
            "SLE-Product-SLES_15-SP6-LTSS, SLE-Product-HPC_15-SP5-LTSS"
        );
        assert_eq!(string_list_field(o, "srcrpms"), "dovecot23");
        assert_eq!(string_list_field(o, "bugs"), "bnc#1276794, CVE-2026-27859");
        assert_eq!(scalar_field(o, "creator"), "amattiazzo");
        // Requested spellings resolve; matching is case/separator-insensitive.
        for name in [
            "Products", "products", "SRCRPMs", "srcrpms", "SRC_RPMS", "Bugs", "BUGS", "Creator",
            "creator",
        ] {
            resolve_field(name).unwrap();
        }
        let specs: Vec<&FieldSpec> = ["Products", "SRCRPMs", "Bugs", "Creator"]
            .iter()
            .map(|n| resolve_field(n).unwrap())
            .collect();
        let rendered = render_fields(&row, &specs);
        for line in [
            "Products: SLE-Product-SLES_15-SP6-LTSS, SLE-Product-HPC_15-SP5-LTSS",
            "SRCRPMs: dovecot23",
            "Bugs: bnc#1276794, CVE-2026-27859",
            "Creator: amattiazzo",
        ] {
            assert!(rendered.contains(line), "missing '{line}' in:\n{rendered}");
        }
    }

    #[test]
    fn queue_list_fields_missing_keys_render_empty_without_panicking() {
        let row = serde_json::json!({"id": "SUSE:Maintenance:77:707"});
        let o = row.as_object().unwrap();
        assert_eq!(string_list_field(o, "products"), "-");
        assert_eq!(string_list_field(o, "srcrpms"), "-");
        assert_eq!(string_list_field(o, "bugs"), "-");
        assert_eq!(scalar_field(o, "creator"), "-");
        // Null, wrong shape, empty and non-string elements degrade, never panic.
        let row = serde_json::json!({
            "products": null,
            "srcrpms": "dovecot23",
            "bugs": ["bnc#1", 42, null, "", "CVE-1"],
            "creator": null,
        });
        let o = row.as_object().unwrap();
        assert_eq!(string_list_field(o, "products"), "-");
        assert_eq!(string_list_field(o, "srcrpms"), "-");
        assert_eq!(string_list_field(o, "bugs"), "bnc#1, CVE-1");
        assert_eq!(scalar_field(o, "creator"), "-");
        let row = serde_json::json!({"products": [], "bugs": []});
        let o = row.as_object().unwrap();
        assert_eq!(string_list_field(o, "products"), "-");
        assert_eq!(string_list_field(o, "bugs"), "-");
    }

    // ------------------------------------------------------------------ json

    #[tokio::test]
    async fn json_dumps_full_rows_untouched() {
        // --json is the everything-the-server-sent path, so a key the renderer
        // knows nothing about must survive.
        let mut row = distinct_row();
        row.as_object_mut()
            .unwrap()
            .insert("zz_future_field".to_owned(), serde_json::json!("kept"));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [row]})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // No prose header — stdout is the JSON document.
        assert!(!out.contains("Update queue"), "{out}");
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let rows = parsed.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["zz_future_field"], "kept");
        // Types survive too: a stringified 421 would betray a re-render.
        assert_eq!(rows[0]["priority"], serde_json::json!(421));
    }

    #[tokio::test]
    async fn unassigned_roles_without_assignment_data_is_na_not_all_open() {
        // A classic row under `--status all` has `review_groups` but no
        // `assignees`; rendering every group as open would be indistinguishable
        // from the truth, so n/a — the per-row degradation that keeps
        // `Unassigned Roles` allowed under `--status all`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 7, "status": "accepted_merged", "kind": "Maintenance",
                     "id": "SUSE:Maintenance:11:111",
                     "review_groups": ["qam-sle", "qam-openqa"]},
                ]})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all", "-F", "Unassigned Roles"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Unassigned Roles: n/a"), "{out}");
        assert!(!out.contains("qam-sle"), "false open-group list: {out}");

        // With the assignees key present (even empty) the derivation runs.
        let obj = serde_json::json!({
            "assignees": {},
            "review_groups": ["qam-sle", "qam-openqa"],
        });
        assert_eq!(
            extract_unassigned_roles(obj.as_object().unwrap()),
            "qam-sle, qam-openqa"
        );
    }

    #[test]
    fn empty_assignee_entry_list_counts_as_unassigned() {
        // {"qam-sle": []} must appear in Unassigned Roles, not vanish.
        let obj = serde_json::json!({
            "assignees": {"qam-sle": []},
            "review_groups": ["qam-sle", "qam-openqa"],
        });
        let o = obj.as_object().unwrap();
        assert_eq!(extract_assigned_roles(o), "-");
        assert_eq!(extract_unassigned_roles(o), "qam-sle, qam-openqa");
    }

    #[tokio::test]
    async fn json_empty_queue_prints_empty_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert_eq!(out.trim(), "[]");
        assert!(!out.contains("No updates in the queue"), "{out}");
    }

    #[test]
    fn json_conflicts_with_field_selection() {
        let base = clap::Command::new("updates").no_binary_name(true);
        let cmd = Updates.configure(base);
        assert!(
            cmd.clone()
                .try_get_matches_from(["--json", "-F", "Rating"])
                .is_err()
        );
        assert!(cmd.try_get_matches_from(["--json"]).is_ok());
    }

    // ------------------------------------------------------------ row budget

    /// 150-row queue with one mid-queue non-`testing` anomaly and exact duplicates.
    fn crush_fixture() -> Vec<serde_json::Value> {
        let mut rows: Vec<serde_json::Value> = (0..150)
            .map(|i| {
                serde_json::json!({
                    "priority": 1, "status": "testing", "kind": "Maintenance",
                    "id": format!("row-{i:03}"),
                })
            })
            .collect();
        rows[100] = serde_json::json!({
            "priority": 1, "status": "failed", "kind": "Maintenance",
            "id": "row-anomaly",
        });
        rows.push(rows[0].clone());
        rows.push(rows[1].clone());
        rows
    }

    #[tokio::test]
    async fn row_budget_crushes_human_queue_and_keeps_anomaly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": crush_fixture()})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("row-anomaly"), "anomaly must survive: {out}");
        assert!(!out.contains("row-060"), "middle normal row dropped: {out}");
        assert!(out.contains("row-000") && out.contains("row-149"), "{out}");
        assert!(
            out.lines()
                .last()
                .is_some_and(|l| l.starts_with("…[truncated")),
            "{out}"
        );
        assert!(out.contains("--limit/--offset/--field/-G"), "{out}");
        assert!(
            out.contains(" of 150"),
            "deduped total in notice/header: {out}"
        );
    }

    #[tokio::test]
    async fn row_budget_json_emits_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": crush_fixture()})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all", "--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // Envelope keeps stdout valid JSON: naive parse must succeed.
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let obj = parsed.as_object().unwrap();
        assert!(obj.contains_key("rows"), "{out}");
        assert!(obj.contains_key("truncation"), "{out}");
        let trunc = &obj["truncation"];
        assert_eq!(trunc["total"], 150, "{out}");
        assert!(trunc["truncated"].as_u64().unwrap() > 0, "{out}");
        assert!(
            obj["truncation"]["hint"]
                .as_str()
                .unwrap()
                .contains("--limit"),
            "{out}"
        );
        let rows = obj["rows"].as_array().unwrap();
        assert!(
            rows.len() <= super::super::row_budget::ROW_CAP,
            "{}",
            rows.len()
        );
        assert!(rows.iter().any(|r| r["id"] == "row-anomaly"), "{out}");
        assert!(!rows.iter().any(|r| r["id"] == "row-060"), "{out}");
        assert!(!out.contains("…[truncated"), "no trailing plaintext: {out}");
    }

    #[test]
    fn json_help_mentions_truncation() {
        let base = clap::Command::new("updates").no_binary_name(true);
        let mut cmd = Updates.configure(base);
        let help = cmd.render_help().to_string();
        assert!(help.contains("truncation"), "{help}");
        assert!(help.contains("envelope"), "{help}");
        assert!(help.contains("valid JSON"), "{help}");
    }

    #[test]
    fn update_key_is_id_status_priority_with_idless_fallback() {
        // Same identity fields dedup even when the rest differs (coarser than
        // full-row serialisation, by design: one id is one update).
        let a = serde_json::json!({"id": "x", "status": "testing", "priority": 1, "title": "one"});
        let b = serde_json::json!({"id": "x", "status": "testing", "priority": 1, "title": "two"});
        assert_eq!(update_key(&a), update_key(&b));
        let c = serde_json::json!({"id": "y", "status": "testing", "priority": 1});
        assert_ne!(update_key(&a), update_key(&c));
        // Id-less rows keep exact-dedup: distinct rows stay distinct.
        let u1 = serde_json::json!({"status": "testing", "priority": 1, "title": "one"});
        let u2 = serde_json::json!({"status": "testing", "priority": 1, "title": "two"});
        assert_ne!(update_key(&u1), update_key(&u2));
        assert_eq!(update_key(&u1), update_key(&u1.clone()));
    }

    #[test]
    fn null_id_falls_back_to_full_row_key() {
        // Explicit `"id": null` must not collapse distinct rows into one
        // `Some("null")` bucket.
        let n1 =
            serde_json::json!({"id": null, "status": "testing", "priority": 1, "title": "one"});
        let n2 =
            serde_json::json!({"id": null, "status": "testing", "priority": 1, "title": "two"});
        assert_ne!(update_key(&n1), update_key(&n2));
        assert_eq!(update_key(&n1), update_key(&n1.clone()));
    }

    #[test]
    fn unknown_status_is_anomaly_keep() {
        for row in [
            serde_json::json!({"id": "a"}),
            serde_json::json!({"id": "b", "status": null}),
            serde_json::json!({"id": "c", "status": "weird"}),
            serde_json::json!({"id": "d", "status": 5}),
        ] {
            assert_eq!(anomaly_severity(&row), 1, "{row}");
        }
        assert_eq!(
            anomaly_severity(&serde_json::json!({"status": "testing"})),
            0
        );
        assert_eq!(
            anomaly_severity(&serde_json::json!({"status": "failed"})),
            2
        );
        assert_eq!(
            anomaly_severity(&serde_json::json!({"status": "blocked"})),
            2
        );
    }

    #[test]
    fn severe_update_outranks_mild_when_overflowing() {
        // 300 testing rows except a mild ("weird") early-middle and a severe
        // ("failed") late-middle: only 50 middle anomalies fit, so the severe
        // survives and the mild drops.
        let mut rows: Vec<serde_json::Value> = (0..300)
            .map(|i| {
                serde_json::json!({
                    "priority": 1, "status": "testing", "kind": "Maintenance",
                    "id": format!("row-{i:03}"),
                })
            })
            .collect();
        rows[280] = serde_json::json!({
            "priority": 1, "status": "weird", "kind": "Maintenance", "id": "row-mild",
        });
        rows[250] = serde_json::json!({
            "priority": 1, "status": "failed", "kind": "Maintenance", "id": "row-severe",
        });
        for (i, row) in rows.iter_mut().enumerate().take(292).skip(41) {
            if i == 250 || i == 280 {
                continue;
            }
            *row = serde_json::json!({
                "priority": 1, "status": "weird", "kind": "Maintenance",
                "id": format!("row-{i:03}"),
            });
        }
        let out = crush(rows, update_key, anomaly_severity);
        let ids: Vec<_> = out
            .kept
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"row-severe"), "severe survives: {ids:?}");
        assert!(!ids.contains(&"row-mild"), "mild drops: {ids:?}");
        assert_eq!((out.anomaly_kept, out.anomaly_total), (52, 251));
    }

    #[tokio::test]
    async fn unknown_status_rows_survive_crush() {
        let mut rows: Vec<serde_json::Value> = (0..150)
            .map(|i| {
                serde_json::json!({
                    "priority": 1, "status": "testing", "kind": "Maintenance",
                    "id": format!("row-{i:03}"),
                })
            })
            .collect();
        rows[100] = serde_json::json!({"priority": 1, "kind": "Maintenance", "id": "row-nostatus"});
        rows[101] = serde_json::json!({"priority": 1, "status": null, "kind": "Maintenance", "id": "row-null"});
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": rows})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all", "--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let rows = parsed["rows"].as_array().unwrap();
        let kept: Vec<_> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap_or("?"))
            .collect();
        assert!(kept.contains(&"row-nostatus"), "{out}");
        assert!(kept.contains(&"row-null"), "{out}");
    }

    #[tokio::test]
    async fn middle_row_recoverable_via_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updates": crush_fixture()})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &["--status", "all", "--offset", "50", "--limit", "50"],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("row-060"),
            "paged middle row must appear: {out}"
        );
        assert!(
            !out.contains("…[truncated"),
            "50-row window fits budget: {out}"
        );
    }

    #[tokio::test]
    async fn boundary_100_plain_array_101_envelope() {
        for (n, expect_envelope, expect_kept) in [(100, false, 100), (101, true, 50)] {
            let rows: Vec<serde_json::Value> = (0..n)
                .map(|i| {
                    serde_json::json!({
                        "priority": 1, "status": "testing", "kind": "Maintenance",
                        "id": format!("row-{i:03}"),
                    })
                })
                .collect();
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/updates"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": rows})),
                )
                .mount(&server)
                .await;
            let (mut session, buf) = teregen_session(&server);
            let args = matches(&Updates, &["--status", "all", "--json"]);
            Updates.call(&mut session, &args).await.unwrap();
            let out = buf.contents();
            let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            if expect_envelope {
                let obj = parsed.as_object().unwrap();
                assert!(obj.contains_key("rows"), "n={n}: {out}");
                assert!(obj.contains_key("truncation"), "n={n}: {out}");
                assert_eq!(
                    obj["rows"].as_array().unwrap().len(),
                    expect_kept,
                    "n={n}: {out}"
                );
                assert!(!out.contains("…[truncated"), "n={n}: {out}");
            } else {
                assert!(parsed.is_array(), "n={n}: {out}");
                assert_eq!(
                    parsed.as_array().unwrap().len(),
                    expect_kept,
                    "n={n}: {out}"
                );
            }
        }
    }

    #[tokio::test]
    async fn json_bracket_id_survives_in_plain_array() {
        // A data id containing "[truncated" must survive verbatim; stdout stays
        // one valid JSON document with no stripping.
        let rows = vec![serde_json::json!({
            "priority": 1, "status": "testing", "kind": "Maintenance",
            "id": "row-[truncated]-fake",
        })];
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": rows})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["--status", "all", "--json"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(parsed[0]["id"], "row-[truncated]-fake", "{out}");
    }
}
