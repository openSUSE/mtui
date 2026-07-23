//! XML parsers for the OBS payloads the native QAM ops need (no `osc`).
//!
//! Ported from upstream `mtui/data_sources/obs/models.py`. Small, explicit
//! `quick-xml` parsers over the `withfullhistory=1` request document, the
//! `group?login` directory, and the `MAINT:RejectReason` attribute envelope.
//! The request parser exposes each review's NESTED `<history>`
//! (`withfullhistory` puts the assignment events inside each `<review>`), which
//! the assignment state machine in [`crate::obs`] inference (G1e) replays.
//!
//! **Security (DTD/XXE guard, PR#323):** OBS never sends a `<!DOCTYPE>` /
//! `<!ENTITY>`, so every parser refuses one *before* parsing. This neutralises
//! an entity-expansion DoS (billion-laughs / quadratic blowup) from a
//! compromised or MITM'd (`ssl_verify=false`) response, without pulling in a
//! third-party XML parser. Defence in depth: `quick-xml` does not expand general
//! entities anyway (it surfaces them as distinct events), so a DTD-free body
//! with an entity reference never expands either. This mirrors the guard in
//! [`crate::obs::client::error_summary`].

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::obs::errors::ObsError;

/// IBS ignores these automation groups when deciding what counts as a QAM group.
const IGNORED_QAM_GROUPS: [&str; 2] = ["qam-auto", "qam-openqa"];

/// The `MAINT:RejectReason` attribute namespace.
pub(crate) const REJECT_REASON_NAMESPACE: &str = "MAINT";
/// The `MAINT:RejectReason` attribute name.
pub(crate) const REJECT_REASON_NAME: &str = "RejectReason";

/// IBS QAM-group test: starts with `qam`, minus the automation groups.
#[must_use]
pub(crate) fn is_qam_group(name: &str) -> bool {
    name.starts_with("qam") && !IGNORED_QAM_GROUPS.contains(&name)
}

/// One `<history>` entry of a review (who did what, when).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// The actor of the event (`who` attribute).
    pub(crate) who: String,
    /// The timestamp of the event (`when` attribute).
    pub(crate) when: String,
    /// The `<description>` text, trimmed.
    pub(crate) description: String,
}

/// One `<review>` of a request (a group or user review + its history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// The review `state` (e.g. `new`, `accepted`).
    pub(crate) state: String,
    /// The reviewing group (`by_group`), when the review targets a group.
    pub(crate) by_group: Option<String>,
    /// The reviewing user (`by_user`), when the review targets a user.
    pub(crate) by_user: Option<String>,
    /// The nested `<history>` events (`withfullhistory=1`).
    pub(crate) history: Vec<HistoryEvent>,
}

/// The parts of a request the QAM ops need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The request id (`id` attribute).
    pub(crate) reqid: String,
    /// The overall request `state/@name`.
    pub(crate) state: String,
    /// The `action/source/@project`, when present.
    pub(crate) src_project: Option<String>,
    /// The request's reviews.
    pub(crate) reviews: Vec<Review>,
}

/// Refuse an OBS document carrying a DTD, before any parsing.
///
/// Reproduces upstream `models._fromstring`'s pre-parse guard: a body carrying
/// `<!DOCTYPE` or `<!ENTITY` is rejected. The message contains `"DTD"`.
fn refuse_dtd(xml: &str) -> Result<(), ObsError> {
    if xml.contains("<!DOCTYPE") || xml.contains("<!ENTITY") {
        return Err(ObsError::Parse(
            "refusing to parse an OBS document that carries a DTD".to_owned(),
        ));
    }
    Ok(())
}

/// Build a DTD-refusing reader with text trimming, mirroring `error_summary`.
fn reader(xml: &str) -> Result<Reader<&[u8]>, ObsError> {
    refuse_dtd(xml)?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    Ok(reader)
}

/// Map any `quick-xml` reader failure to a typed [`ObsError::Parse`].
fn parse_err(context: &str) -> ObsError {
    ObsError::Parse(format!("malformed OBS {context} XML"))
}

/// Read attribute `key` off a start/empty tag as an owned `String`, if present.
fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == key {
            // `normalized_value` resolves standard XML character references but
            // NOT DTD-defined entities — and the DTD guard has already refused
            // any `<!ENTITY>`, so an attribute entity-expansion vector cannot
            // reach here. `XmlVersion::default()` is the implicit 1.0 the OBS
            // API speaks.
            return a
                .normalized_value(XmlVersion::default())
                .ok()
                .map(|v| v.into_owned());
        }
    }
    None
}

/// Parse one `<review …>…</review>` subtree, starting *after* its start event.
///
/// `start` is the review's start tag (for its attributes); the reader is
/// positioned just inside the element. Consumes through the matching `</review>`.
fn parse_review(
    reader: &mut Reader<&[u8]>,
    buf: &mut Vec<u8>,
    start: &quick_xml::events::BytesStart<'_>,
) -> Result<Review, ObsError> {
    let mut review = Review {
        state: attr(start, b"state").unwrap_or_default(),
        by_group: attr(start, b"by_group"),
        by_user: attr(start, b"by_user"),
        history: Vec::new(),
    };

    loop {
        buf.clear();
        match reader
            .read_event_into(buf)
            .map_err(|_| parse_err("request"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"history" => {
                let ev = parse_history(reader, &mut Vec::new(), &e)?;
                review.history.push(ev);
            }
            Event::Empty(e) if e.local_name().as_ref() == b"history" => {
                review.history.push(HistoryEvent {
                    who: attr(&e, b"who").unwrap_or_default(),
                    when: attr(&e, b"when").unwrap_or_default(),
                    description: String::new(),
                });
            }
            Event::End(e) if e.local_name().as_ref() == b"review" => break,
            Event::Eof => return Err(parse_err("request")),
            _ => {}
        }
    }
    Ok(review)
}

/// Parse one `<history …>…</history>` subtree, reading its `<description>` text.
fn parse_history(
    reader: &mut Reader<&[u8]>,
    buf: &mut Vec<u8>,
    start: &quick_xml::events::BytesStart<'_>,
) -> Result<HistoryEvent, ObsError> {
    let mut ev = HistoryEvent {
        who: attr(start, b"who").unwrap_or_default(),
        when: attr(start, b"when").unwrap_or_default(),
        description: String::new(),
    };

    let mut in_description = false;
    loop {
        buf.clear();
        match reader
            .read_event_into(buf)
            .map_err(|_| parse_err("request"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"description" => {
                in_description = true;
            }
            Event::Text(e) if in_description => {
                let text = e.decode().map_err(|_| parse_err("request"))?;
                ev.description.push_str(text.as_ref());
            }
            Event::End(e) if e.local_name().as_ref() == b"description" => {
                in_description = false;
            }
            Event::End(e) if e.local_name().as_ref() == b"history" => break,
            Event::Eof => return Err(parse_err("request")),
            _ => {}
        }
    }
    ev.description = ev.description.trim().to_owned();
    Ok(ev)
}

/// Parse one `<request …>` subtree already opened as `start`, up to `</request>`.
///
/// The reader is positioned just inside the request element. Reads the top-level
/// `<state name=>`, `<action><source project=>` and each `<review>`.
fn parse_request_element(
    reader: &mut Reader<&[u8]>,
    buf: &mut Vec<u8>,
    start: &quick_xml::events::BytesStart<'_>,
) -> Result<Request, ObsError> {
    let mut request = Request {
        reqid: attr(start, b"id").unwrap_or_default(),
        state: String::new(),
        src_project: None,
        reviews: Vec::new(),
    };

    let mut in_action = false;
    loop {
        buf.clear();
        match reader
            .read_event_into(buf)
            .map_err(|_| parse_err("request"))?
        {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"state" => {
                request.state = attr(&e, b"name").unwrap_or_default();
            }
            Event::Start(e) if e.local_name().as_ref() == b"action" => in_action = true,
            Event::End(e) if e.local_name().as_ref() == b"action" => in_action = false,
            Event::Start(e) | Event::Empty(e)
                if in_action && e.local_name().as_ref() == b"source" =>
            {
                if request.src_project.is_none() {
                    request.src_project = attr(&e, b"project");
                }
            }
            Event::Start(e) if e.local_name().as_ref() == b"review" => {
                let review = parse_review(reader, &mut Vec::new(), &e)?;
                request.reviews.push(review);
            }
            Event::Empty(e) if e.local_name().as_ref() == b"review" => {
                request.reviews.push(Review {
                    state: attr(&e, b"state").unwrap_or_default(),
                    by_group: attr(&e, b"by_group"),
                    by_user: attr(&e, b"by_user"),
                    history: Vec::new(),
                });
            }
            Event::End(e) if e.local_name().as_ref() == b"request" => break,
            Event::Eof => return Err(parse_err("request")),
            _ => {}
        }
    }
    Ok(request)
}

/// Parse a `request?withfullhistory=1` document.
///
/// # Errors
///
/// Returns [`ObsError::Parse`] if the body carries a DTD or is malformed.
pub(crate) fn parse_request(xml: &str) -> Result<Request, ObsError> {
    let mut reader = reader(xml)?;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .map_err(|_| parse_err("request"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"request" => {
                return parse_request_element(&mut reader, &mut Vec::new(), &e);
            }
            Event::Empty(e) if e.local_name().as_ref() == b"request" => {
                return Ok(Request {
                    reqid: attr(&e, b"id").unwrap_or_default(),
                    state: String::new(),
                    src_project: None,
                    reviews: Vec::new(),
                });
            }
            Event::Eof => return Err(parse_err("request")),
            _ => {}
        }
    }
}

/// Parse a `<collection>` of requests (the previous-reject search).
///
/// # Errors
///
/// Returns [`ObsError::Parse`] if the body carries a DTD or is malformed.
pub(crate) fn parse_request_collection(xml: &str) -> Result<Vec<Request>, ObsError> {
    let mut reader = reader(xml)?;
    let mut buf = Vec::new();
    let mut requests = Vec::new();
    loop {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .map_err(|_| parse_err("collection"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"request" => {
                requests.push(parse_request_element(&mut reader, &mut Vec::new(), &e)?);
            }
            Event::Empty(e) if e.local_name().as_ref() == b"request" => {
                requests.push(Request {
                    reqid: attr(&e, b"id").unwrap_or_default(),
                    state: String::new(),
                    src_project: None,
                    reviews: Vec::new(),
                });
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(requests)
}

/// Parse a `group?login=<user>` directory into its group names.
///
/// # Errors
///
/// Returns [`ObsError::Parse`] if the body carries a DTD or is malformed.
pub(crate) fn parse_group_directory(xml: &str) -> Result<Vec<String>, ObsError> {
    let mut reader = reader(xml)?;
    let mut buf = Vec::new();
    let mut names = Vec::new();
    loop {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .map_err(|_| parse_err("group directory"))?
        {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"entry" => {
                if let Some(name) = attr(&e, b"name") {
                    names.push(name);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(names)
}

/// Parse the `MAINT:RejectReason` attribute doc into its value list.
///
/// Tolerates the empty `<attributes/>` OBS returns when the attribute is unset
/// (returns `[]`). Values are trimmed; blank values are dropped.
///
/// # Errors
///
/// Returns [`ObsError::Parse`] if the body carries a DTD or is malformed.
pub(crate) fn parse_reject_reason_values(xml: &str) -> Result<Vec<String>, ObsError> {
    let mut reader = reader(xml)?;
    let mut buf = Vec::new();
    let mut values = Vec::new();
    let mut in_value = false;
    let mut current = String::new();
    loop {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .map_err(|_| parse_err("reject reason"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"value" => {
                in_value = true;
                current.clear();
            }
            Event::Text(e) if in_value => {
                let text = e.decode().map_err(|_| parse_err("reject reason"))?;
                current.push_str(text.as_ref());
            }
            Event::End(e) if e.local_name().as_ref() == b"value" => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    values.push(trimmed.to_owned());
                }
                in_value = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(values)
}

/// Build the `<attributes>` POST body for `MAINT:RejectReason`.
///
/// Mirrors upstream `models.build_reject_reason_body`'s serialised element.
#[must_use]
pub(crate) fn build_reject_reason_body(values: &[String]) -> String {
    let mut body = format!(
        "<attributes><attribute name=\"{REJECT_REASON_NAME}\" namespace=\"{REJECT_REASON_NAMESPACE}\">"
    );
    for val in values {
        body.push_str("<value>");
        body.push_str(&xml_escape(val));
        body.push_str("</value>");
    }
    body.push_str("</attribute></attributes>");
    body
}

/// Minimal XML text escaping for reject-reason values.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST_XML: &str = r#"
<request id="42">
  <action type="maintenance_release">
    <source project="SUSE:Maintenance:130" package="pkg.SUSE_SLE-12_Update"/>
    <target project="SUSE:SLE-12:Update" package="pkg.130"/>
  </action>
  <review state="review" when="2014-11-14T11:12:53" who="anon" by_user="anon"/>
  <review state="accepted" by_group="qam-sle">
    <history who="alice" when="2017-09-06T08:06:39">
      <description>Review got accepted</description>
      <comment>review for group qam-sle</comment>
    </history>
  </review>
  <review state="new" by_group="qam-cloud"/>
  <state name="review" who="anon" when="2014-12-01T14:46:23"/>
</request>
"#;

    #[test]
    fn parse_request_core_fields() {
        let req = parse_request(REQUEST_XML).unwrap();
        assert_eq!(req.reqid, "42");
        assert_eq!(req.state, "review");
        assert_eq!(req.src_project.as_deref(), Some("SUSE:Maintenance:130"));
        assert_eq!(req.reviews.len(), 3);
    }

    #[test]
    fn parse_request_reviews_and_nested_history() {
        let req = parse_request(REQUEST_XML).unwrap();
        let sle = req
            .reviews
            .iter()
            .find(|r| r.by_group.as_deref() == Some("qam-sle"))
            .unwrap();
        assert_eq!(sle.state, "accepted");
        assert_eq!(sle.history.len(), 1);
        assert_eq!(sle.history[0].who, "alice");
        assert_eq!(sle.history[0].when, "2017-09-06T08:06:39");
        assert_eq!(sle.history[0].description, "Review got accepted");
        let user_review = req
            .reviews
            .iter()
            .find(|r| r.by_user.as_deref() == Some("anon"))
            .unwrap();
        assert_eq!(user_review.by_group, None);
    }

    #[test]
    fn parse_request_self_closing_history() {
        // A `withfullhistory` review may carry a self-closing `<history/>`
        // (attributes only, no `<description>`): the event is recorded with an
        // empty description.
        let xml = concat!(
            r#"<request id="7">"#,
            r#"<review state="new" by_group="qam-sle">"#,
            r#"<history who="bob" when="2020-01-01T00:00:00"/>"#,
            r#"</review></request>"#
        );
        let req = parse_request(xml).unwrap();
        let ev = &req.reviews[0].history[0];
        assert_eq!(ev.who, "bob");
        assert_eq!(ev.when, "2020-01-01T00:00:00");
        assert_eq!(ev.description, "");
    }

    #[test]
    fn parse_request_malformed_xml_is_parse_error() {
        // A truncated/mismatched document surfaces as a typed ObsError::Parse.
        let err = parse_request(r#"<request id="1"><review></request>"#).unwrap_err();
        assert!(err.to_string().contains("malformed OBS request"), "{err}");
    }

    #[test]
    fn parse_request_truncated_before_request_close_is_error() {
        // A request body cut off before `</request>` (dropped connection /
        // truncated response) must not parse as a partial success.
        let err = parse_request(r#"<request id="1"><state name="review"/>"#).unwrap_err();
        assert!(err.to_string().contains("malformed OBS request"), "{err}");
    }

    #[test]
    fn parse_request_truncated_inside_review_is_error() {
        // EOF before the review's `</review>` is rejected (parse_review guard).
        let err = parse_request(r#"<request id="1"><review state="new" by_group="qam-sle">"#)
            .unwrap_err();
        assert!(err.to_string().contains("malformed OBS request"), "{err}");
    }

    #[test]
    fn parse_request_truncated_inside_history_is_error() {
        // EOF before the history's `</history>` is rejected (parse_history guard).
        let err = parse_request(concat!(
            r#"<request id="1"><review state="new" by_group="qam-sle">"#,
            r#"<history who="bob" when="2020-01-01T00:00:00"><description>hi"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("malformed OBS request"), "{err}");
    }

    #[test]
    fn toplevel_parsers_still_tolerate_natural_eof() {
        // Regression guard: the out-of-scope top-level loops (no single wrapping
        // element) legitimately end on EOF and must keep parsing cleanly.
        assert!(parse_group_directory(r#"<directory count="0"/>"#).is_ok());
        assert!(parse_reject_reason_values("<attributes/>").is_ok());
        assert!(parse_request_collection("<collection/>").is_ok());
        // And a complete, well-formed request still parses.
        assert!(parse_request(REQUEST_XML).is_ok());
    }

    #[test]
    fn parse_request_missing_source_and_state() {
        let req = parse_request(r#"<request id="9"></request>"#).unwrap();
        assert_eq!(req.src_project, None);
        assert_eq!(req.state, "");
        assert_eq!(req.reviews, Vec::new());
    }

    #[test]
    fn parse_group_directory_names() {
        let xml =
            r#"<directory count="2"><entry name="qam-sle"/><entry name="qam-cloud"/></directory>"#;
        assert_eq!(
            parse_group_directory(xml).unwrap(),
            vec!["qam-sle".to_owned(), "qam-cloud".to_owned()]
        );
    }

    #[test]
    fn parse_group_directory_empty() {
        assert_eq!(
            parse_group_directory(r#"<directory count="0"/>"#).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn parse_reject_reason_values_trimmed() {
        let xml = concat!(
            r#"<attributes><attribute namespace="MAINT" name="RejectReason">"#,
            "<value>100:not_fixed</value><value> 101:regression </value>",
            "</attribute></attributes>"
        );
        assert_eq!(
            parse_reject_reason_values(xml).unwrap(),
            vec!["100:not_fixed".to_owned(), "101:regression".to_owned()]
        );
    }

    #[test]
    fn parse_reject_reason_empty_attributes() {
        assert_eq!(
            parse_reject_reason_values("<attributes/>").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn build_reject_reason_body_roundtrips() {
        let body =
            build_reject_reason_body(&["100:not_fixed".to_owned(), "200:regression".to_owned()]);
        assert!(body.contains(r#"namespace="MAINT""#));
        assert!(body.contains(r#"name="RejectReason""#));
        assert_eq!(
            parse_reject_reason_values(&body).unwrap(),
            vec!["100:not_fixed".to_owned(), "200:regression".to_owned()]
        );
    }

    #[test]
    fn is_qam_group_membership() {
        assert!(is_qam_group("qam-sle"));
        assert!(!is_qam_group("qam-auto"));
        assert!(!is_qam_group("qam-openqa"));
        assert!(!is_qam_group("legal-auto"));
    }

    /// A DTD (billion-laughs vector) is refused before parsing, by every parser.
    #[test]
    fn parsers_refuse_dtd() {
        let billion_laughs = concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<!DOCTYPE lolz [<!ENTITY lol "lol">"#,
            r#"<!ENTITY lol2 "&lol;&lol;&lol;">]>"#,
            r#"<request><state name="&lol2;"/></request>"#
        );

        let e = parse_request(billion_laughs).unwrap_err();
        assert!(e.to_string().contains("DTD"), "{e}");
        let e = parse_request_collection(billion_laughs).unwrap_err();
        assert!(e.to_string().contains("DTD"), "{e}");
        let e = parse_group_directory(billion_laughs).unwrap_err();
        assert!(e.to_string().contains("DTD"), "{e}");
        let e = parse_reject_reason_values(billion_laughs).unwrap_err();
        assert!(e.to_string().contains("DTD"), "{e}");
    }
}
