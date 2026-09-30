//! Hand-written `report_*` MCP tools over the report **document**.
//!
//! Where the `testreport_*` tools edit the report as text, these read and
//! write it as data: one named section, or one issue, at a time. A write is
//! validated locally (a typed parse of the whole candidate document, plus a
//! check that no key is silently dropped) and changes local state only;
//! `commit` uploads it.
//!
//! Only a report loaded from a document has one; any other report refuses with
//! a pointer at `testreport_*`.

use mtui_core::ReportAccess;
use mtui_testreport::TestReportBase;
use mtui_types::report_document::{Section, SectionWriteError};
use serde_json::{Map, Value, json};

use crate::session::{
    DEFAULT_PROGRESS_INTERVAL, McpCommandError, McpSession, ProgressSink, run_with_heartbeat,
};
use crate::slim::cap_output;
use crate::tools::ToolDescriptor;
use crate::transfer_tools::resolve_rrid;

const TEMPLATE_NOTE: &str = "Pass `template=<rrid>` to target a specific loaded template; required when more \
     than one is loaded.";

const DOCUMENT_NOTE: &str =
    "Needs a report loaded from a document; an SVN-path report refuses (use testreport_*).";

const WRITE_NOTE: &str = "Validated against the report schema locally, refused with the offending pointers, and \
     applied to local state only; `commit` uploads it.";

fn refuse(msg: impl Into<String>) -> McpCommandError {
    McpCommandError {
        stdout: String::new(),
        stderr: msg.into(),
        exit_code: 1,
    }
}

fn no_document() -> McpCommandError {
    refuse("no report document loaded (SVN-path report); use testreport_*")
}

fn access_refusal(access: ReportAccess, rrid: &str) -> McpCommandError {
    match access {
        ReportAccess::NotLoaded => refuse(format!("template not loaded: {rrid}")),
        ReportAccess::Busy => refuse(format!("template busy: {rrid}")),
    }
}

fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Map<String, Value> {
    let mut properties = Map::new();
    for (name, spec) in props {
        properties.insert(name.to_owned(), spec);
    }
    let mut s = Map::new();
    s.insert("type".to_owned(), Value::String("object".to_owned()));
    s.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        s.insert(
            "required".to_owned(),
            Value::Array(required.iter().map(|r| json!(r)).collect()),
        );
    }
    s.insert("additionalProperties".to_owned(), Value::Bool(false));
    s
}

/// The five document tool descriptors (transport-free).
#[must_use]
pub fn document_tool_descriptors() -> Vec<ToolDescriptor> {
    let template_prop = || {
        json!({
            "type": "string",
            "description": "RRID of a loaded template to target (required when >1 loaded).",
        })
    };
    let section_prop = |sections: Vec<Section>, what: &str| {
        json!({
            "type": "string",
            "enum": sections.into_iter().map(Section::as_str).collect::<Vec<_>>(),
            "description": what,
        })
    };
    let issue_id_prop = |what: &str| json!({ "type": "string", "description": what });
    let value_prop = |what: &str| json!({ "description": what });

    let sections = ToolDescriptor {
        name: "report_sections".to_owned(),
        description: format!(
            "List the report document's sections (`name`, byte `size` — advisory, \
             `has_unanswered`) and its completeness: `complete`, and `unfilled`, the \
             RFC 6901 pointer of every null leaf. `dirty` is true while edits are not \
             yet uploaded by `commit`. {DOCUMENT_NOTE} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(vec![("template", template_prop())], &[]),
        read_only: true,
    };

    let section_read = ToolDescriptor {
        name: "report_section_read".to_owned(),
        description: format!(
            "Read one section of the report document as JSON (`data`). {DOCUMENT_NOTE} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "section",
                    section_prop(Section::ALL.to_vec(), "Section to read."),
                ),
                ("template", template_prop()),
            ],
            &["section"],
        ),
        read_only: true,
    };

    let writable: Vec<Section> = Section::ALL
        .into_iter()
        .filter(|s| s.is_tester_writable())
        .collect();
    let section_write = ToolDescriptor {
        name: "report_section_write".to_owned(),
        description: format!(
            "Replace one tester section of the report document with `value`. `update` and \
             `install` are pipeline-owned and refused; a section the report lacks (`review` \
             on a PI report) is refused; an `issues` write must keep exactly the same issue \
             keys (use report_issue_write for one entry). {WRITE_NOTE} A later `export` \
             overwrites testing.install, testing.openqa and people.testers. \
             {DOCUMENT_NOTE} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "section",
                    section_prop(writable, "Tester section to replace."),
                ),
                (
                    "value",
                    value_prop(
                        "The section's new JSON value, shaped as report_section_read returns it.",
                    ),
                ),
                ("template", template_prop()),
            ],
            &["section", "value"],
        ),
        read_only: false,
    };

    let issue_read = ToolDescriptor {
        name: "report_issue_read".to_owned(),
        description: format!(
            "Without `issue_id`, list the report's issues (`id`, `title`, `status`, \
             `severity`); with it, read that issue in full. {DOCUMENT_NOTE} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "issue_id",
                    issue_id_prop("Issue key such as `bsc#1234567`; omit to list all."),
                ),
                ("template", template_prop()),
            ],
            &[],
        ),
        read_only: true,
    };

    let issue_write = ToolDescriptor {
        name: "report_issue_write".to_owned(),
        description: format!(
            "Replace one existing issue of the report document with `value`; the issue set \
             cannot grow or shrink. {WRITE_NOTE} {DOCUMENT_NOTE} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "issue_id",
                    issue_id_prop("Key of an existing issue, such as `bsc#1234567`."),
                ),
                (
                    "value",
                    value_prop(
                        "The issue's new JSON value, shaped as report_issue_read returns it.",
                    ),
                ),
                ("template", template_prop()),
            ],
            &["issue_id", "value"],
        ),
        read_only: false,
    };

    vec![
        sections,
        section_read,
        section_write,
        issue_read,
        issue_write,
    ]
}

/// Dispatch a document tool by name, heartbeat-wrapped when a sink is given.
///
/// # Errors
/// [`McpCommandError`] for an unknown tool, a bad argument, no loaded
/// document, a busy template, or a refused write.
pub async fn dispatch_document_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
) -> Result<Value, McpCommandError> {
    let body = dispatch_document_tool_inner(session, name, kwargs);
    match sink {
        None => body.await,
        Some(sink) => run_with_heartbeat(body, sink, name, DEFAULT_PROGRESS_INTERVAL).await,
    }
}

async fn dispatch_document_tool_inner(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
) -> Result<Value, McpCommandError> {
    if let Some(desc) = document_tool_descriptors()
        .into_iter()
        .find(|d| d.name == name)
    {
        let allowed = desc
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|props| props.keys().map(String::as_str));
        crate::tools::reject_unknown_kwargs(kwargs, allowed)?;
    }

    let template = opt_str(kwargs, "template")?;
    let out = match name {
        "report_sections" => report_sections(session, template).await?,
        "report_section_read" => {
            let section = section_arg(kwargs)?;
            report_section_read(session, section, template).await?
        }
        "report_section_write" => {
            let section = section_arg(kwargs)?;
            let value = value_arg(kwargs)?;
            report_section_write(session, section, value, template).await?
        }
        "report_issue_read" => {
            report_issue_read(session, opt_str(kwargs, "issue_id")?, template).await?
        }
        "report_issue_write" => {
            let issue_id =
                opt_str(kwargs, "issue_id")?.ok_or_else(|| refuse("`issue_id` is required"))?;
            let value = value_arg(kwargs)?;
            report_issue_write(session, issue_id, value, template).await?
        }
        other => return Err(refuse(format!("unknown document tool: {other}"))),
    };
    Ok(capped(session, out))
}

fn opt_str<'a>(
    kwargs: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, McpCommandError> {
    match kwargs.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(other) => Err(refuse(format!("{key} must be a string, got {other}"))),
    }
}

fn section_arg(kwargs: &Map<String, Value>) -> Result<Section, McpCommandError> {
    opt_str(kwargs, "section")?
        .ok_or_else(|| refuse("`section` is required"))?
        .parse()
        .map_err(|e: mtui_types::report_document::UnknownSectionError| refuse(e.to_string()))
}

/// `value` is any JSON, `null` included (an unanswered `verdict`), so only its
/// absence is refused.
fn value_arg(kwargs: &Map<String, Value>) -> Result<Value, McpCommandError> {
    kwargs
        .get("value")
        .cloned()
        .ok_or_else(|| refuse("`value` is required"))
}

/// Bounds an over-budget result without breaking its JSON: the capped text
/// travels in `content`, flagged, with the full byte count.
fn capped(session: &McpSession, value: Value) -> Value {
    let text = value.to_string();
    let cap = session.max_output_bytes();
    if cap == 0 || text.len() <= cap {
        return value;
    }
    json!({ "truncated": true, "size": text.len(), "content": cap_output(text, cap) })
}

/// Runs `f` on the target report's base state under the template's scoped
/// lock, with the session mutex held only for the closure.
async fn read_base<R>(
    session: &McpSession,
    template: Option<&str>,
    f: impl FnOnce(&TestReportBase) -> Result<R, McpCommandError>,
) -> Result<R, McpCommandError> {
    let _scope = session.scoped_lock(template).await;
    let guard = session.session().lock().await;
    let rrid = resolve_rrid(&guard, template)?;
    guard
        .with_report(&rrid, |report| f(report.base()))
        .map_err(|access| access_refusal(access, &rrid))?
}

/// The mutable counterpart of [`read_base`].
async fn write_base<R>(
    session: &McpSession,
    template: Option<&str>,
    f: impl FnOnce(&mut TestReportBase) -> Result<R, McpCommandError>,
) -> Result<R, McpCommandError> {
    let _scope = session.scoped_lock(template).await;
    let mut guard = session.session().lock().await;
    let rrid = resolve_rrid(&guard, template)?;
    guard
        .with_report_mut(&rrid, |report| f(report.base_mut()))
        .map_err(|access| access_refusal(access, &rrid))?
}

async fn report_sections(
    session: &McpSession,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    read_base(session, template, |base| {
        let doc = base.document.as_ref().ok_or_else(no_document)?;
        let completeness = doc.completeness();
        Ok(json!({
            "id": doc.id,
            "dirty": base.document_dirty,
            "sections": doc.section_summaries(),
            "complete": completeness.complete,
            "unfilled": completeness.unfilled,
        }))
    })
    .await
}

async fn report_section_read(
    session: &McpSession,
    section: Section,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    read_base(session, template, |base| {
        let doc = base.document.as_ref().ok_or_else(no_document)?;
        let data = doc
            .section(section)
            .ok_or_else(|| refuse(format!("section {section} is not present in this report")))?;
        Ok(json!({ "id": doc.id, "section": section.as_str(), "data": data }))
    })
    .await
}

async fn report_issue_read(
    session: &McpSession,
    issue_id: Option<&str>,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    read_base(session, template, |base| {
        let doc = base.document.as_ref().ok_or_else(no_document)?;
        let issues = doc.section(Section::Issues).unwrap_or(Value::Null);
        let Some(issue_id) = issue_id else {
            let index: Vec<Value> = issues
                .as_object()
                .into_iter()
                .flatten()
                .map(|(id, issue)| {
                    json!({
                        "id": id,
                        "title": issue.get("title").unwrap_or(&Value::Null),
                        "status": issue.get("status").unwrap_or(&Value::Null),
                        "severity": issue.get("severity").unwrap_or(&Value::Null),
                    })
                })
                .collect();
            return Ok(json!({ "id": doc.id, "issues": index }));
        };
        let issue = issues
            .get(issue_id)
            .ok_or_else(|| refuse(format!("no issue {issue_id:?} in this report")))?;
        Ok(json!({ "id": doc.id, "issue_id": issue_id, "issue": issue }))
    })
    .await
}

async fn report_section_write(
    session: &McpSession,
    section: Section,
    value: Value,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    write_base(session, template, |base| {
        let doc = base.document.as_ref().ok_or_else(no_document)?;
        let updated = doc.with_section(section, value).map_err(write_refusal)?;
        let size = updated
            .section(section)
            .map_or(0, |data| data.to_string().len());
        let id = updated.id.clone();
        base.document = Some(updated);
        base.mark_document_authored(&[&format!("/{}", section.as_str())]);
        Ok(json!({ "id": id, "section": section.as_str(), "dirty": true, "size": size }))
    })
    .await
}

async fn report_issue_write(
    session: &McpSession,
    issue_id: &str,
    value: Value,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    write_base(session, template, |base| {
        let doc = base.document.as_ref().ok_or_else(no_document)?;
        let updated = doc.with_issue(issue_id, value).map_err(write_refusal)?;
        let id = updated.id.clone();
        base.document = Some(updated);
        base.mark_document_authored(&[&format!("/issues/{issue_id}")]);
        Ok(json!({ "id": id, "issue_id": issue_id, "dirty": true }))
    })
    .await
}

fn write_refusal(err: SectionWriteError) -> McpCommandError {
    refuse(err.to_string())
}
