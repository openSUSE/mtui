use mtui_tui::{App, Exit, Form, Schema};
use mtui_types::report_document::{ReportDocument, SCHEMA_JSON, Verdict};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};

const MAXIMAL: &str = mtui_types::fixtures::MAXIMAL;
const OPENQA_L3: &str = mtui_types::fixtures::MAINTENANCE_OPENQA_L3;

fn doc(raw: &str) -> ReportDocument {
    raw.parse().expect("fixture parses")
}

fn app(raw: &str) -> App {
    App::new(Form::new(&Schema::load().unwrap(), &doc(raw)))
}

fn app_with_schema(schema: &Value, raw: &str) -> App {
    App::new(Form::new(
        &Schema::parse(&schema.to_string()).unwrap(),
        &doc(raw),
    ))
}

fn shipped_schema() -> Value {
    serde_json::from_str(SCHEMA_JSON).unwrap()
}

fn press(app: &mut App, code: KeyCode) -> Option<Exit> {
    app.handle(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(app: &mut App, c: char) -> Option<Exit> {
    app.handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn screen(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn the_first_screen_lists_the_summary_fields() {
    let mut app = app(OPENQA_L3);

    insta::assert_snapshot!(screen(&mut app));
}

#[test]
fn an_open_enum_picker_lists_its_choices() {
    let mut app = app(OPENQA_L3);

    press(&mut app, KeyCode::Enter);

    insta::assert_snapshot!(screen(&mut app));
}

#[test]
fn the_comment_editor_is_a_multi_line_modal() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "first line");
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "second line");

    insta::assert_snapshot!(screen(&mut app));
}

#[test]
fn quitting_with_unsaved_changes_asks_first() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Right);

    assert_eq!(press(&mut app, KeyCode::Char('q')), None);

    insta::assert_snapshot!(screen(&mut app));
}

#[test]
fn a_refused_save_jumps_to_the_field_at_fault() {
    // The walker is looser than the typed model here, so the refusal comes
    // from the save, on a field that is not on the first screen.
    let mut schema = shipped_schema();
    schema["$defs"]["issue"]["properties"]["status"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("BOGUS"));
    let mut form = Form::new(&Schema::parse(&schema.to_string()).unwrap(), &doc(MAXIMAL));
    form.set("/issues/bsc#1234567/status", json!("BOGUS"))
        .unwrap();
    let mut app = App::new(form);

    assert_eq!(ctrl(&mut app, 's'), None);

    insta::assert_snapshot!(screen(&mut app));
}

#[test]
fn a_verdict_edit_ends_in_a_saved_document() {
    let mut app = app(OPENQA_L3);

    press(&mut app, KeyCode::Right);
    let exit = ctrl(&mut app, 's');

    let Some(Exit::Saved(saved)) = exit else {
        panic!("not saved: {exit:?}");
    };
    assert_eq!(*saved.document.verdict, Some(Verdict::Passed));
    assert_eq!(saved.touched, ["/verdict"]);
}

#[test]
fn the_picker_sets_the_chosen_value() {
    let mut app = app(OPENQA_L3);

    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let exit = ctrl(&mut app, 's');

    let Some(Exit::Saved(saved)) = exit else {
        panic!("not saved: {exit:?}");
    };
    assert_eq!(*saved.document.verdict, Some(Verdict::Failed));
}

#[test]
fn escape_leaves_the_picker_without_a_change() {
    let mut app = app(OPENQA_L3);

    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Esc);

    assert!(screen(&mut app).contains("[unchanged]"));
}

#[test]
fn quitting_while_dirty_asks_and_n_keeps_the_editor_open() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Right);

    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    assert!(screen(&mut app).contains("Discard unsaved changes?"));
    assert_eq!(press(&mut app, KeyCode::Char('n')), None);

    let after = screen(&mut app);
    assert!(!after.contains("Discard unsaved changes?"));
    assert!(after.contains("[modified]"));
}

#[test]
fn confirming_the_quit_discards() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('q'));

    assert_eq!(press(&mut app, KeyCode::Char('y')), Some(Exit::Discarded));
}

#[test]
fn a_second_ctrl_c_at_the_confirmation_discards() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Right);

    assert_eq!(ctrl(&mut app, 'c'), None);
    assert_eq!(ctrl(&mut app, 'c'), Some(Exit::Discarded));
}

#[test]
fn ctrl_c_on_a_clean_form_discards_at_once() {
    let mut app = app(OPENQA_L3);

    assert_eq!(ctrl(&mut app, 'c'), Some(Exit::Discarded));
}

#[test]
fn q_on_a_clean_form_discards_at_once() {
    let mut app = app(OPENQA_L3);

    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Exit::Discarded));
}

#[test]
fn a_key_release_does_nothing() {
    let mut app = app(OPENQA_L3);
    let mut release = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;

    assert_eq!(app.handle(release), None);

    assert!(screen(&mut app).contains("[unchanged]"));
}

#[test]
fn typed_text_lands_in_the_field_on_accept() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "all good");
    ctrl(&mut app, 's');

    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };
    assert_eq!(saved.document.comment.as_deref(), Some("all good"));
    assert_eq!(saved.touched, ["/comment"]);
}

#[test]
fn escape_cancels_a_text_edit() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "discarded");
    press(&mut app, KeyCode::Esc);

    assert!(screen(&mut app).contains("[unchanged]"));
}

#[test]
fn text_that_breaks_a_constraint_keeps_the_editor_open_with_the_reason() {
    let mut schema = shipped_schema();
    schema["properties"]["comment"]["pattern"] = json!("^[a-z ]+$");
    let mut app = app_with_schema(&schema, OPENQA_L3);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "SHOUT");

    ctrl(&mut app, 's');

    let screen = screen(&mut app);
    assert!(screen.contains("must match ^[a-z ]+$"), "{screen}");
    assert!(screen.contains("Ctrl-S accept"), "still editing: {screen}");
}

#[test]
fn a_read_only_field_says_so_and_does_not_open() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);

    press(&mut app, KeyCode::Enter);

    let screen = screen(&mut app);
    assert!(
        screen.contains("/issues/bnc#1253357/title: read-only"),
        "{screen}"
    );
    assert!(screen.contains("[unchanged]"));
    assert!(screen.contains("Tab section"), "no editor opened: {screen}");
}

#[test]
fn n_sets_the_field_to_null() {
    let mut app = app(MAXIMAL);

    press(&mut app, KeyCode::Char('n'));
    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };

    assert_eq!(*saved.document.verdict, None);
}

#[test]
fn tab_wraps_around_the_sections_in_both_directions() {
    let mut app = app(OPENQA_L3);
    press(&mut app, KeyCode::BackTab);

    assert!(screen(&mut app).contains("install (read-only)"));
    let first = screen(&mut app);
    for _ in 0..7 {
        press(&mut app, KeyCode::Tab);
    }
    assert_eq!(screen(&mut app), first);
}

#[test]
fn enter_creates_an_absent_regression_block() {
    let mut app = app(OPENQA_L3);
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab);
    }
    let before = screen(&mut app);
    assert!(before.contains("regression"), "{before}");
    assert!(before.contains("(absent, Enter creates it)"), "{before}");
    press(&mut app, KeyCode::End);

    press(&mut app, KeyCode::Enter);

    let after = screen(&mut app);
    assert!(!after.contains("(absent, Enter creates it)"), "{after}");
    assert!(after.contains("[modified]"));
}

#[test]
fn enter_toggles_a_group() {
    let mut app = app(MAXIMAL);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    let open = screen(&mut app);
    assert!(open.contains("[-] bsc#1234567"), "{open}");

    press(&mut app, KeyCode::Enter);

    let closed = screen(&mut app);
    assert!(closed.contains("[+] bsc#1234567"), "{closed}");
    assert!(!closed.contains("reproducer"));
}

#[test]
fn the_help_overlay_names_the_export_caveat_and_any_key_closes_it() {
    let mut app = app(OPENQA_L3);

    press(&mut app, KeyCode::Char('?'));
    assert!(screen(&mut app).contains("Run export before editing"));
    press(&mut app, KeyCode::Char('x'));

    assert!(!screen(&mut app).contains("Run export before editing"));
}

// ---- tester-filed issues ---------------------------------------------- //

const NEW_ISSUES: &str = mtui_types::fixtures::MAINTENANCE_NEW_ISSUES;

fn on_issues_tab(raw: &str) -> App {
    let mut app = app(raw);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    app
}

fn file_bug(app: &mut App, id: &str, title: &str, comment: &str) {
    press(app, KeyCode::Char('a'));
    for line in [id, title, comment] {
        type_text(app, line);
        press(app, KeyCode::Enter);
    }
}

#[test]
fn a_bug_filed_with_a_ends_in_a_saved_new_issue() {
    let mut app = on_issues_tab(NEW_ISSUES);

    file_bug(&mut app, "bsc#100", "found a crash", "on s390x");

    let screen = screen(&mut app);
    assert!(screen.contains("found a crash"), "{screen}");
    assert!(screen.contains("[modified]"), "{screen}");
    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };
    let issue = &saved.document.issues[&"bsc#100".parse().unwrap()];
    assert_eq!(
        (issue.new, issue.title.as_str()),
        (Some(true), "found a crash")
    );
    assert_eq!(issue.comment.as_deref(), Some("on s390x"));
    assert_eq!(saved.touched, ["/new_issues"]);
}

#[test]
fn the_add_prompt_refuses_a_bad_id_and_an_empty_title_and_escape_cancels() {
    let mut app = on_issues_tab(NEW_ISSUES);

    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "cve#1");
    press(&mut app, KeyCode::Enter);
    let asking = screen(&mut app);
    assert!(asking.contains("new issue: id"), "{asking}");
    assert!(asking.contains("must match"), "{asking}");

    press(&mut app, KeyCode::Esc);
    assert!(!screen(&mut app).contains("new issue:"));
    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };
    assert!(saved.touched.is_empty(), "{:?}", saved.touched);
}

#[test]
fn an_empty_title_keeps_the_prompt_open() {
    let mut app = on_issues_tab(NEW_ISSUES);
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "bsc#7");
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::Enter);

    let screen = screen(&mut app);
    assert!(screen.contains("new issue: title"), "{screen}");
    assert!(screen.contains("title must not be empty"), "{screen}");
}

#[test]
fn a_is_refused_outside_the_issues_tab() {
    let mut app = app(NEW_ISSUES);

    press(&mut app, KeyCode::Char('a'));

    let screen = screen(&mut app);
    assert!(!screen.contains("new issue:"), "{screen}");
    assert!(
        screen.contains("bugs are filed from the issues tab"),
        "{screen}"
    );
}

#[test]
fn d_deletes_a_filed_bug_and_refuses_a_pipeline_issue() {
    let mut app = on_issues_tab(NEW_ISSUES);

    press(&mut app, KeyCode::Char('d'));
    let refused = screen(&mut app);
    assert!(
        refused.contains("only tester-filed issues can be deleted"),
        "{refused}"
    );
    assert!(refused.contains("[unchanged]"), "{refused}");

    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Char('d'));
    assert!(screen(&mut app).contains("[modified]"));
    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };
    assert!(
        !saved
            .document
            .issues
            .contains_key(&"bsc#999".parse().unwrap())
    );
}

#[test]
fn the_issues_tab_hints_the_bug_keys_and_help_names_them() {
    let mut app = on_issues_tab(NEW_ISSUES);
    let screen_ = screen(&mut app);
    assert!(screen_.contains("a add  d del"), "{screen_}");

    press(&mut app, KeyCode::Char('?'));
    let help = screen(&mut app);
    assert!(help.contains("file a new bug"), "{help}");
    assert!(help.contains("delete a bug you filed"), "{help}");
}

#[test]
fn enter_on_the_new_issues_row_opens_the_add_prompt() {
    let mut app = on_issues_tab(NEW_ISSUES);
    let (_, row) = Form::new(&Schema::load().unwrap(), &doc(NEW_ISSUES))
        .locate("/new_issues")
        .unwrap();
    for _ in 0..row {
        press(&mut app, KeyCode::Down);
    }

    press(&mut app, KeyCode::Enter);
    for line in ["bsc#100", "found a crash", "on s390x"] {
        type_text(&mut app, line);
        press(&mut app, KeyCode::Enter);
    }

    let Some(Exit::Saved(saved)) = ctrl(&mut app, 's') else {
        panic!("not saved");
    };
    assert_eq!(saved.touched, ["/new_issues"]);
    assert!(
        saved
            .document
            .issues
            .contains_key(&"bsc#100".parse().unwrap())
    );
}
