use mtui_tui::form::Widget;
use mtui_tui::{Form, Schema};
use mtui_types::report_document::{ReportDocument, SCHEMA_JSON, Verdict};
use serde_json::{Value, json};

const MAXIMAL: &str = include_str!("../../mtui-types/tests/fixtures/document/maximal.json");
const PI: &str = include_str!("../../mtui-types/tests/fixtures/document/pi.json");
const OPENQA_L3: &str =
    include_str!("../../mtui-types/tests/fixtures/document/maintenance_openqa_l3.json");

fn doc(raw: &str) -> ReportDocument {
    raw.parse().expect("fixture parses")
}

fn form(raw: &str) -> Form {
    Form::new(&Schema::load().unwrap(), &doc(raw))
}

fn tab_names(form: &Form) -> Vec<&'static str> {
    form.tabs().iter().map(|t| t.name).collect()
}

fn field(form: &Form, pointer: &str) -> mtui_tui::Field {
    (0..form.tabs().len())
        .flat_map(|tab| form.fields(tab))
        .find(|f| f.pointer == pointer)
        .unwrap_or_else(|| panic!("no field {pointer}"))
}

#[test]
fn a_verdict_edit_saves_as_a_document_with_one_touched_section() {
    let mut form = form(OPENQA_L3);

    form.set("/verdict", json!("PASSED")).unwrap();
    let saved = form.save().unwrap();

    assert_eq!(*saved.document.verdict, Some(Verdict::Passed));
    assert_eq!(saved.touched, ["/verdict"]);
}

#[test]
fn an_untouched_form_saves_a_byte_equal_document() {
    for raw in [MAXIMAL, PI, OPENQA_L3] {
        let original = doc(raw);
        let form = Form::new(&Schema::load().unwrap(), &original);

        let saved = form.save().unwrap();

        assert!(saved.touched.is_empty());
        assert_eq!(
            serde_json::to_string(&saved.document).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
        assert!(!form.is_dirty());
    }
}

#[test]
fn a_tristate_cycles_null_true_false_and_back() {
    let mut form = form(MAXIMAL);
    let pointer = "/review/source/untracked_changes";
    assert_eq!(field(&form, pointer).value, json!(false));

    form.cycle(pointer, true).unwrap();
    assert_eq!(field(&form, pointer).value, Value::Null);
    form.cycle(pointer, true).unwrap();
    assert_eq!(field(&form, pointer).value, json!(true));
    form.cycle(pointer, true).unwrap();
    assert_eq!(field(&form, pointer).value, json!(false));

    assert!(!form.is_dirty(), "a full turn lands on the starting value");
    form.cycle(pointer, false).unwrap();
    assert_eq!(field(&form, pointer).value, json!(true));
}

#[test]
fn an_issue_status_set_to_null_is_written_as_null_not_dropped() {
    let mut form = form(MAXIMAL);

    form.set_null("/issues/bsc#1234567/status").unwrap();
    let saved = form.save().unwrap();

    let value = serde_json::to_value(&saved.document).unwrap();
    assert_eq!(value["issues"]["bsc#1234567"]["status"], Value::Null);
    assert!(
        value["issues"]["bsc#1234567"]
            .as_object()
            .unwrap()
            .contains_key("status")
    );
    assert_eq!(saved.touched, ["/issues"]);
}

#[test]
fn a_pi_has_no_review_tab_and_no_review_fields() {
    let form = form(PI);

    assert!(!tab_names(&form).contains(&"review"));
    assert!(
        (0..form.tabs().len())
            .flat_map(|tab| form.fields(tab))
            .all(|f| !f.pointer.starts_with("/review"))
    );
}

#[test]
fn a_maintenance_report_lists_writable_tabs_before_read_only_ones() {
    let form = form(MAXIMAL);

    assert_eq!(
        tab_names(&form),
        [
            "summary", "people", "issues", "testing", "review", "update", "install"
        ]
    );
    let writable: Vec<_> = form.tabs().iter().map(|t| t.writable).collect();
    assert_eq!(writable, [true, true, true, true, true, false, false]);
}

#[test]
fn a_regression_block_can_be_created_but_an_install_block_cannot() {
    let mut form = form(OPENQA_L3);
    assert_eq!(
        field(&form, "/testing/regression").widget,
        Widget::Absent { creatable: true }
    );
    assert_eq!(
        field(&form, "/testing/install").widget,
        Widget::Absent { creatable: false }
    );

    form.create("/testing/regression").unwrap();
    let refused = form.create("/testing/install").unwrap_err();

    assert_eq!(refused.pointer, "/testing/install");
    assert!(
        refused.message.contains("export builds it"),
        "{}",
        refused.message
    );
    let saved = form.save().unwrap();
    assert_eq!(saved.touched, ["/testing"]);
    let regression = saved.document.testing.regression.expect("created");
    assert!(regression.verdict.is_none() && regression.comment.is_none());
    assert!(saved.document.testing.install.is_none());
    assert_eq!(
        field(&form, "/testing/regression/verdict").widget,
        Widget::Picker { nullable: true }
    );
}

#[test]
fn creating_what_is_already_there_is_refused() {
    let mut form = form(MAXIMAL);

    assert!(form.create("/testing/regression").is_err());
}

#[test]
fn a_read_only_leaf_refuses_an_edit_and_stays_put() {
    let mut form = form(MAXIMAL);

    for pointer in [
        "/issues/bsc#1234567/title",
        "/people/testers",
        "/update/packager",
        "/testing/openqa/install/verdict",
        "/review/build_log/results",
    ] {
        let err = form.set(pointer, json!("x")).unwrap_err();
        assert_eq!(err.message, "read-only", "{pointer}");
    }
    assert!(!form.is_dirty());
    assert!(!field(&form, "/issues/bsc#1234567/title").editable);
    assert!(field(&form, "/issues/bsc#1234567/status").editable);
}

#[test]
fn a_value_outside_an_enum_is_refused_at_the_field() {
    let mut form = form(MAXIMAL);

    let err = form.set("/verdict", json!("MAYBE")).unwrap_err();

    assert_eq!(err.pointer, "/verdict");
    assert!(err.message.contains("PASSED, FAILED"), "{}", err.message);
    assert!(!form.is_dirty());
}

#[test]
fn a_null_is_refused_where_the_schema_does_not_allow_one() {
    let mut form = form(MAXIMAL);

    // `/review/build_log/results` is read-only, so use a field that is
    // editable but whose node is not nullable: none exists in the shipped
    // schema, hence the pattern-bearing copy below.
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["comment"]["type"] = json!("string");
    let strict = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));
    let mut strict = strict;

    assert_eq!(
        strict.set_null("/comment").unwrap_err().message,
        "cannot be null"
    );
    assert!(form.set_null("/comment").is_ok());
}

#[test]
fn text_that_breaks_a_pattern_is_refused_at_the_field() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["comment"]["pattern"] = json!("^[a-z ]+$");
    let mut form = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));

    let err = form.set_text("/comment", "SHOUTING!").unwrap_err();

    assert_eq!(err.pointer, "/comment");
    assert!(err.message.contains("^[a-z ]+$"), "{}", err.message);
    form.set_text("/comment", "calm words").unwrap();
    assert_eq!(field(&form, "/comment").value, json!("calm words"));
}

#[test]
fn text_shorter_than_min_length_is_refused_at_the_field() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["comment"]["minLength"] = json!(3);
    let mut form = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));

    assert!(
        form.set_text("/comment", "ab")
            .unwrap_err()
            .message
            .contains("at least 3")
    );
    assert!(form.set_text("/comment", "abc").is_ok());
}

#[test]
fn an_integer_field_refuses_text_that_is_not_an_integer() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["comment"] = json!({"type": ["integer", "null"]});
    let mut form = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));

    assert_eq!(
        form.set_text("/comment", "4x").unwrap_err().message,
        "must be an integer"
    );
    assert_eq!(
        form.set("/comment", json!(1.5)).unwrap_err().message,
        "must be an integer"
    );
    form.set_text("/comment", " 42 ").unwrap();
    assert_eq!(field(&form, "/comment").value, json!(42));
}

#[test]
fn a_save_the_document_refuses_names_the_offending_field() {
    // The walker is looser than the typed model here: it offers a status the
    // document cannot hold, so the refusal comes from the save.
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["$defs"]["issue"]["properties"]["status"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("BOGUS"));
    let mut form = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));
    form.set("/verdict", json!("FAILED")).unwrap();
    form.set("/issues/bsc#1234567/status", json!("BOGUS"))
        .unwrap();

    let err = form.save().unwrap_err();

    assert_eq!(err.pointer, "/issues/bsc#1234567/status");
}

#[test]
fn a_comment_field_is_multi_line_and_other_text_is_not() {
    let form = form(MAXIMAL);

    assert_eq!(
        field(&form, "/comment").widget,
        Widget::Text {
            multiline: true,
            nullable: true
        }
    );
    assert_eq!(
        field(&form, "/people/reviewer/name").widget,
        Widget::Text {
            multiline: false,
            nullable: true
        }
    );
}

#[test]
fn unfilled_counts_the_null_leaves_left_after_edits() {
    let mut form = form(OPENQA_L3);
    let before = form.unfilled();

    form.set("/verdict", json!("PASSED")).unwrap();

    assert_eq!(form.unfilled(), before - 1);
}

#[test]
fn open_groups_list_keys_the_schema_does_not_declare_as_fixed_rows() {
    let form = form(MAXIMAL);

    let extra = field(&form, "/testing/an_unknown_testing_key");

    assert_eq!(extra.widget, Widget::Fixed);
    assert!(!extra.editable);
}

#[test]
fn a_collapsed_group_hides_its_children_until_toggled() {
    let mut form = form(MAXIMAL);
    let tab = form
        .tabs()
        .iter()
        .position(|t| t.name == "testing")
        .unwrap();
    let pointer = "/testing/install/checks/0/target";
    let shown = |form: &Form| {
        form.fields(tab)
            .iter()
            .any(|f| f.pointer == format!("{pointer}/arch"))
    };
    assert!(form.fields(tab).iter().any(|f| f.pointer == pointer));
    assert!(!shown(&form));

    form.toggle_group(pointer);
    assert!(shown(&form));

    form.toggle_group(pointer);
    assert!(!shown(&form));
}

#[test]
fn locate_opens_the_groups_above_a_hidden_field() {
    let mut form = form(MAXIMAL);

    let pointer = "/testing/install/checks/0/before/examplepkg";
    let tab = form
        .tabs()
        .iter()
        .position(|t| t.name == "testing")
        .unwrap();
    assert!(!form.fields(tab).iter().any(|f| f.pointer == pointer));

    let (found, row) = form.locate(pointer).unwrap();

    assert_eq!(found, tab);
    assert_eq!(form.fields(tab)[row].pointer, pointer);
}

#[test]
fn locate_falls_back_to_the_nearest_enclosing_field() {
    let mut form = form(MAXIMAL);

    let (tab, row) = form.locate("/people/reviewer/nothing/here").unwrap();

    assert_eq!(form.fields(tab)[row].pointer, "/people/reviewer");
}
