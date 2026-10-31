//! The `updates` command — list the update queue via the TeReGen API.

use std::collections::HashSet;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use futures::stream::{self, StreamExt};
use mtui_config::Config;
use mtui_datasources::obs::Osc;
use mtui_datasources::obs::models::ObsComment;
use mtui_datasources::obs::qam;
use mtui_datasources::{HttpClient, UpdatesQuery};
use mtui_types::{RequestKind, RequestReviewID};
use serde_json::Value;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
#[cfg(test)]
use crate::display::TRUNCATION_MARK;
use crate::display::sanitize_external;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

use super::row_budget::{crush, json_envelope, row_notice, truncation_meta};

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
/// assignment view (dropping the unassigned default); `--status all` widens to
/// every status. Session-global, so [`Scope::Single`] rather than a per-template
/// fan-out — though it may issue one TeReGen query per `--review-group`.
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
        cmd.arg(
            Arg::new("review_group")
                .short('G')
                .long("review-group")
                .value_name("GROUP")
                .action(ArgAction::Append)
                .help(
                    "filter by review group as the bare group name, e.g. qam-sle \
                     (not the '<group>-review' login form, which classic rows lack); \
                     repeatable — groups are OR-ed (one server query per group)",
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
                     non-OBS (SLFO) rows render n/a, comments are sanitised \
                     and capped per row",
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
                .help("filter by status (default: testing); use 'all' for every status"),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .default_value("0")
                .help("cap the number of rows after --offset (0 = all)"),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .default_value("0")
                .help("skip the first N rows (0 = from the start); with --limit, page any middle slice"),
        )
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
                .help("filter to updates assigned to the current session user"),
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
        // Dedup so `-G a -G a` takes the single-query path.
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
        let limit = args.get_one::<usize>("limit").copied().unwrap_or(0);
        let offset = args.get_one::<usize>("offset").copied().unwrap_or(0);
        let mine = args.get_flag("mine");
        let all_assignees = args.get_flag("all_assignees");

        let assignee = if mine {
            Some(session.config.session_user.clone())
        } else {
            args.get_one::<String>("assignee").cloned()
        };

        // '--status all' widens by sending no status filter at all.
        let status_all = status_arg == STATUS_ALL;
        let status = if status_all { None } else { Some(status_arg) };

        // Default view is the unassigned pickup queue; --assignee/--mine and
        // --all-assignees/--status all opt out of that filter.
        let chose_other_view = assignee.is_some() || all_assignees;
        let unassigned = !chose_other_view && !status_all;
        // Show the assignee column whenever assignment is part of the view.
        let want_assignment = assignee.is_some() || unassigned || all_assignees;

        // Without `with_assignment` TeReGen omits assignee/assignees on SLFO
        // rows, so an assignment-derived field would render a false
        // "unassigned"; forcing it implies status=testing server-side,
        // contradicting `--status all`.
        if !want_assignment && let Some(spec) = specs.iter().find(|s| s.needs_assignment) {
            return Err(CommandError::Other(format!(
                "field '{}' needs assignment data, which TeReGen omits under \
                 '--status all'; drop --status all or pick an assignment view \
                 (--assignee/--mine/--all-assignees)",
                spec.canonical
            )));
        }

        let teregen = teregen_client(session)?;
        // A named fn, not a closure: a closure cannot tie its return's
        // lifetime to its argument's.
        fn query_for<'a>(
            group: Option<&'a str>,
            status: Option<&'a str>,
            assignee: Option<&'a str>,
            unassigned: bool,
            with_assignment: bool,
        ) -> UpdatesQuery<'a> {
            UpdatesQuery {
                review_group: group,
                status,
                assignee,
                unassigned,
                with_assignment,
                no_cache: false,
            }
        }

        // One query per group (none = one ungrouped query): TeReGen has no OR
        // semantics for a repeated `review_group` param (the last wins), and the
        // filter cannot be replicated client-side — the server matches SLFO rows
        // against a group without serialising `review_groups` on them.
        let groups: Vec<Option<&str>> = if review_groups.is_empty() {
            vec![None]
        } else {
            review_groups.iter().map(|g| Some(g.as_str())).collect()
        };
        let multi = groups.len() > 1;
        let queries: Vec<UpdatesQuery<'_>> = groups
            .iter()
            .map(|g| {
                query_for(
                    *g,
                    status.as_deref(),
                    assignee.as_deref(),
                    unassigned,
                    want_assignment,
                )
            })
            .collect();
        // Bounded so a scripted thirty-`-G` call does not open thirty
        // connections; `buffered` keeps batches in `-G` order. Materialised
        // eagerly: a lazy `map` closure trips a higher-ranked-lifetime limit.
        let futures: Vec<_> = queries.iter().map(|q| teregen.updates(q)).collect();
        let results: Vec<_> = stream::iter(futures).buffered(4).collect().await;

        // `Err` (transport/API failure) is surfaced, `Ok(None)` (no `updates`
        // key) skipped: only a *successful* empty result is an empty queue.
        let mut rows: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for result in results {
            let batch = result.map_err(|e| {
                CommandError::Other(format!(
                    "Update queue query failed (TeReGen unreachable): {e}"
                ))
            })?;
            let Some(batch) = batch else { continue };
            let batch = batch.as_array().ok_or_else(|| {
                CommandError::Other("Update queue query returned a malformed response".to_owned())
            })?;
            for row in batch {
                // A row can sit in several groups. One without an id is kept,
                // never silently dropped. A single query cannot repeat a row.
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

        if rows.is_empty() {
            // Keep --json output parseable even for an empty queue.
            session.display.println(if as_json {
                "[]"
            } else {
                "No updates in the queue"
            });
            return Ok(());
        }

        // Paging via --offset (pre-crush) makes any middle slice recoverable; chosen over an
        // explicit-window notice as it fits the existing --limit plumbing.
        let windowed: Vec<Value> = {
            let skipped = offset.min(rows.len());
            let mut v: Vec<Value> = rows.into_iter().skip(skipped).collect();
            if limit > 0 && limit < v.len() {
                v.truncate(limit);
            }
            v
        };
        // Row budget backstops `--limit 0=all`: head+tail+anomalies, exact-deduped.
        // Row-cap is not byte-cap: MCP max_output_bytes can still cut mid-array on huge rows.
        let crushed = crush(windowed, update_key, anomaly_severity);
        let mut shown = crushed.kept;
        // Over-cap --json stays in-band like the byte-cap convention, but as a
        // structured envelope so stdout stays valid JSON for jq/MCP parsers.
        let notice = (crushed.truncated > 0).then(|| {
            row_notice(
                crushed.truncated,
                crushed.total,
                crushed.anomaly_kept,
                crushed.anomaly_total,
                UPDATES_HINT,
            )
        });

        if as_json {
            if crushed.truncated > 0 {
                let meta = truncation_meta(
                    crushed.truncated,
                    crushed.total,
                    crushed.anomaly_kept,
                    crushed.anomaly_total,
                    UPDATES_HINT,
                );
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
                session.display.println(&render_row(u, want_assignment));
            }
        } else {
            if needs_obs {
                // The transport builds only when a shown row can be looked up:
                // an all-idless (or all-non-OBS) listing answers without
                // credentials. Non-OBS rows are stamped first so their
                // fields render n/a rather than dashes.
                mark_non_obs_rows(&mut shown);
                let targets = obs_targets(&shown);
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
/// missing/non-array/empty → `-`. Non-string elements are skipped.
fn extract_obs_comments(obj: &serde_json::Map<String, Value>) -> String {
    if obj.get("obs_comments").is_some_and(Value::is_null) {
        return "n/a".to_owned();
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

/// `Unassigned Roles`: `review_groups` minus the groups in `assignees`. TeReGen
/// serialises `review_groups` only on classic Maintenance rows — SLFO rows omit
/// it while still matching a `review_group` filter server-side — so there the
/// honest answer is `n/a`, not an empty list.
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
/// non-OBS row, see [`mark_non_obs_rows`]) renders `n/a`; a missing key (an
/// id-less row, or one OBS refused) renders `-` like the TeReGen-backed
/// lists. Every element goes through [`sanitize_external`]: package names and
/// issue refs are OBS text like comments, and a raw ESC/C1 in either would
/// reach the REPL as a live terminal sequence. Kept separate from
/// [`string_list_field`] so TeReGen nulls keep their dash and TeReGen text
/// keeps its verbatim rendering.
fn obs_string_list_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    if obj.get(key).is_some_and(Value::is_null) {
        return "n/a".to_owned();
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
/// arrived in.
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

/// Stamp the `obs_*` keys null on rows OBS cannot serve, so the OBS-backed
/// extractors render `n/a` (like `Unassigned Roles`) instead of a dash that
/// would read as "looked, found nothing".
fn mark_non_obs_rows(shown: &mut [Value]) {
    for row in shown {
        let non_obs = row
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| RequestReviewID::parse(id).ok())
            .is_some_and(|rrid| !obs_addressable(&rrid));
        if non_obs && let Some(obj) = row.as_object_mut() {
            for key in ["obs_package_streams", "obs_issues", "obs_comments"] {
                obj.insert(key.to_owned(), Value::Null);
            }
        }
    }
}

/// The shown rows addressable on OBS: `(index, RRID)` for rows with a
/// parseable, OBS-addressable `id`, in display order. SLFO rows (Gitea PR
/// numbers, not OBS request ids) never enter, so one row's lookup cannot
/// serve another request's data.
fn obs_targets(shown: &[Value]) -> Vec<(usize, RequestReviewID)> {
    shown
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            row.get("id")
                .and_then(Value::as_str)
                .and_then(|id| RequestReviewID::parse(id).ok())
                .filter(obs_addressable)
                .map(|rrid| (i, rrid))
        })
        .collect()
}

/// Fill the OBS-backed `-F` keys into each shown row, read live from OBS after
/// `--limit`/`--offset` (and the row-budget crush) so a large queue costs one
/// fetch per *shown* row, not per queued row. A row OBS refuses keeps its
/// TeReGen shape and renders dashes; when *no* shown row is servable the
/// command errors (a silent all-dash listing would hide a dead OBS base).
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
    use wiremock::matchers::{method, path, query_param};
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

    #[tokio::test]
    async fn mine_uses_session_user_and_limits_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("assignee", "tester"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                {"priority": 1, "status": "testing", "kind": "Maintenance", "id": "a", "assignee": "tester"},
                {"priority": 2, "status": "testing", "kind": "Maintenance", "id": "b", "assignee": "tester"},
            ]})))
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        config.session_user = "tester".to_owned();
        session.config = config;

        let args = matches(&Updates, &["--mine", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("assignee=tester"), "{out}");
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

    #[tokio::test]
    async fn multi_group_queries_each_group_merges_and_dedups() {
        // One query per group, rows merged priority-descending, and the row
        // present in both groups appearing once.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "qam-sle"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 10, "status": "testing", "kind": "Maintenance", "id": "row-a"},
                    {"priority": 5, "status": "testing", "kind": "Maintenance", "id": "row-both"},
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "qam-teradata"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 7, "status": "testing", "kind": "Maintenance", "id": "row-c"},
                    {"priority": 5, "status": "testing", "kind": "Maintenance", "id": "row-both"},
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &[
                "--review-group",
                "qam-sle",
                "--review-group",
                "qam-teradata",
            ],
        );
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // Three unique rows, not four.
        assert!(out.contains("Update queue (3):"), "{out}");
        assert_eq!(out.matches("row-both").count(), 1, "{out}");
        // Priority-descending across groups: a(10), c(7), both(5).
        let (pa, pc, pb) = (
            out.find("row-a").unwrap(),
            out.find("row-c").unwrap(),
            out.find("row-both").unwrap(),
        );
        assert!(pa < pc && pc < pb, "order wrong: {out}");
        // wiremock verifies the .expect(1) counts on drop.
    }

    #[tokio::test]
    async fn multi_group_partial_failure_is_an_error_not_partial_output() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "good"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 1, "status": "testing", "kind": "Maintenance", "id": "ok-row"},
                ]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "bad"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, buf) = teregen_session(&server);
        let args = matches(
            &Updates,
            &["--review-group", "good", "--review-group", "bad"],
        );
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        // No partial listing was printed.
        assert!(!buf.contents().contains("ok-row"), "{}", buf.contents());
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
        // SLFO rows never serialise review_groups; the honest render is n/a.
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
        assert!(msg.contains("--all-assignees"), "{msg}");

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

    #[tokio::test]
    async fn multi_group_limit_slices_after_merge_sort() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "g1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 1, "status": "testing", "kind": "Maintenance", "id": "low"},
                ]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "g2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                    {"priority": 100, "status": "testing", "kind": "Maintenance", "id": "high"},
                ]})),
            )
            .mount(&server)
            .await;
        let (mut session, buf) = teregen_session(&server);
        let args = matches(&Updates, &["-G", "g1", "-G", "g2", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // A slice taken before the merge-sort would keep "low".
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("high"), "{out}");
        assert!(!out.contains("low"), "{out}");
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

    // ------------------------------------------- OBS fallback (PR3, #415)

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
        // 404s renders dashes while its neighbour serves.
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
        assert!(out.contains("Package-Streams: -"), "{out}");
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
    fn obs_targets_skips_non_obs_rows() {
        // Kills looking a Gitea PR number up as an OBS request: SLFO review
        // ids are PR numbers (architecture.md), so only Maintenance and PI
        // rows are OBS-addressable. Unparseable ids stay skipped as before.
        let row = |id: &str| serde_json::json!({"id": id});
        let rows = [
            row("SUSE:SLFO:1.2:9"),
            row("SUSE:Maintenance:1:56789"),
            row("SUSE:PI:42:7"),
            row("not-an-rrid"),
        ];
        let ids: Vec<u64> = obs_targets(&rows)
            .iter()
            .map(|(_, r)| r.review_id)
            .collect();
        assert_eq!(ids, [56789, 7]);
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
