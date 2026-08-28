//! The `updates` command — list the update queue via the TeReGen API.

use std::collections::HashSet;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use futures::stream::{self, StreamExt};
use mtui_datasources::UpdatesQuery;
use serde_json::Value;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

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
                     update; names are case-insensitive and treat spaces, \
                     hyphens and underscores as equivalent",
                ),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .conflicts_with("field")
                .help(
                    "print the raw TeReGen rows as a JSON array (each row \
                     emitted whole, unlike -F; honours --limit; not combinable \
                     with -F); an empty queue prints []",
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
                .help("cap the number of rows (0 = all)"),
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
        let as_json = args.get_flag("json");
        let status_arg = args
            .get_one::<String>("status")
            .cloned()
            .unwrap_or_else(|| "testing".to_owned());
        let limit = args.get_one::<usize>("limit").copied().unwrap_or(0);
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

        let shown: &[Value] = if limit > 0 && limit < rows.len() {
            &rows[..limit]
        } else {
            &rows
        };

        if as_json {
            // The raw rows, nothing discarded, and no count header: stdout is
            // the JSON document.
            let doc = Value::Array(shown.to_vec());
            session.display.println(
                &serde_json::to_string_pretty(&doc)
                    .expect("serialising a serde_json::Value is infallible"),
            );
            return Ok(());
        }

        session
            .display
            .println(&format!("Update queue ({}):", shown.len()));
        if specs.is_empty() {
            for u in shown {
                session.display.println(&render_row(u, want_assignment));
            }
        } else {
            for (i, u) in shown.iter().enumerate() {
                if i > 0 {
                    session.display.println("");
                }
                session.display.println(&render_fields(u, &specs));
            }
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
    extract: fn(&serde_json::Map<String, Value>) -> String,
}

/// The fields the TeReGen queue listing can serve. osc-qam names are canonical
/// so `osc qam list -F` muscle memory transfers; the raw TeReGen key is an
/// alias where it differs.
static FIELDS: &[FieldSpec] = &[
    FieldSpec {
        canonical: "ReviewRequestID",
        aliases: &["id"],
        needs_assignment: false,
        extract: |o| scalar_field(o, "id"),
    },
    FieldSpec {
        canonical: "Incident Priority",
        aliases: &["priority", "prio"],
        needs_assignment: false,
        extract: |o| scalar_field(o, "priority"),
    },
    FieldSpec {
        canonical: "Rating",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "rating"),
    },
    FieldSpec {
        canonical: "Category",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "category"),
    },
    FieldSpec {
        canonical: "Status",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "status"),
    },
    FieldSpec {
        canonical: "Kind",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "kind"),
    },
    FieldSpec {
        canonical: "Deadline",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "deadline"),
    },
    FieldSpec {
        canonical: "Assignee",
        aliases: &[],
        needs_assignment: true,
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
        extract: extract_assigned_roles,
    },
    FieldSpec {
        canonical: "Unassigned Roles",
        aliases: &[],
        needs_assignment: false,
        extract: extract_unassigned_roles,
    },
    FieldSpec {
        canonical: "Title",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "title"),
    },
    FieldSpec {
        canonical: "URL",
        aliases: &[],
        needs_assignment: false,
        extract: |o| scalar_field(o, "url"),
    },
];

/// osc-qam fields the listing does not carry yet (#415), named so the error can
/// say "known, but not available here" instead of "unknown".
static UNAVAILABLE_FIELDS: &[&str] = &[
    "Products",
    "SRCRPMs",
    "Bugs",
    "Package-Streams",
    "Creator",
    "Issues",
    "Comments",
];

/// Normalizes a field name for matching: lowercase, separators dropped, so
/// `Incident Priority`, `incident-priority` and `incident_priority` coincide.
fn normalize_field(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolves a `-F` name to its spec, or an error naming the #415 gap for a
/// known-but-unservable osc-qam field and listing what is available otherwise.
fn resolve_field(name: &str) -> Result<&'static FieldSpec, CommandError> {
    let norm = normalize_field(name);
    if let Some(spec) = FIELDS
        .iter()
        .find(|s| normalize_field(s.canonical) == norm || s.aliases.iter().any(|a| *a == norm))
    {
        return Ok(spec);
    }
    if let Some(known) = UNAVAILABLE_FIELDS
        .iter()
        .find(|f| normalize_field(f) == norm)
    {
        return Err(CommandError::Other(format!(
            "field '{known}' is not in TeReGen's queue listing yet \
             (tracked in #415); available fields: {}",
            available_field_names()
        )));
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
        // Bare -F: all twelve canonical names, in registry order.
        let names = Updates.complete(&session, "", "updates -F ");
        assert_eq!(names.len(), FIELDS.len(), "{names:?}");
        assert_eq!(names[0], "ReviewRequestID");
        assert!(names.contains(&"Assigned Roles".to_owned()), "{names:?}");
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
    async fn known_but_unserved_field_names_the_gap() {
        let server = MockServer::start().await;
        let (mut session, _buf) = teregen_session(&server);
        let args = matches(&Updates, &["-F", "bugs"]);
        let err = Updates.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'Bugs' is not in TeReGen's queue listing"),
            "{msg}"
        );
        assert!(msg.contains("#415"), "{msg}");
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
}
