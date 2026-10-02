//! The editor screen: a state machine fed key events, drawn onto a ratatui
//! frame. It owns no terminal; the caller supplies events and a backend.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui_textarea::{CursorMove, TextArea};
use serde_json::Value;

use crate::form::{Field, FieldError, Form, Saved, Widget};

/// How the editor ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Exit {
    Saved(Box<Saved>),
    Discarded,
}

enum Mode {
    Browse,
    Edit {
        pointer: String,
        area: Box<TextArea<'static>>,
        multiline: bool,
    },
    Picker {
        pointer: String,
        choices: Vec<Value>,
        index: usize,
    },
    ConfirmQuit,
    Help,
}

const PAGE: usize = 10;
const MAX_LABEL: usize = 40;

pub struct App {
    form: Form,
    tab: usize,
    cursors: Vec<usize>,
    list: ListState,
    mode: Mode,
    status: Option<String>,
    error: Option<FieldError>,
}

impl App {
    #[must_use]
    pub fn new(form: Form) -> Self {
        let cursors = vec![0; form.tabs().len()];
        Self {
            form,
            tab: 0,
            cursors,
            list: ListState::default(),
            mode: Mode::Browse,
            status: None,
            error: None,
        }
    }

    /// Feed one key press; returns the exit when the key ends the session.
    pub fn handle(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.mode {
            Mode::Browse => self.browse(key, ctrl),
            Mode::Edit { .. } => {
                self.edit(key, ctrl);
                None
            }
            Mode::Picker { .. } => {
                self.pick(key, ctrl);
                None
            }
            Mode::ConfirmQuit => match key.code {
                KeyCode::Char('y') => Some(Exit::Discarded),
                KeyCode::Char('c') if ctrl => Some(Exit::Discarded),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    None
                }
                _ => None,
            },
            Mode::Help => {
                self.mode = Mode::Browse;
                None
            }
        }
    }

    fn browse(&mut self, key: KeyEvent, ctrl: bool) -> Option<Exit> {
        let fields = self.form.fields(self.tab);
        let count = fields.len();
        let at = self.cursors[self.tab].min(count.saturating_sub(1));
        match key.code {
            KeyCode::Char('s') if ctrl => return self.save(),
            KeyCode::Char('c') if ctrl => return self.quit(),
            KeyCode::Char('q') if !ctrl => return self.quit(),
            KeyCode::Tab => self.switch_tab(1),
            KeyCode::BackTab => self.switch_tab(-1),
            KeyCode::Up => self.cursors[self.tab] = at.saturating_sub(1),
            KeyCode::Down => self.cursors[self.tab] = (at + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.cursors[self.tab] = at.saturating_sub(PAGE),
            KeyCode::PageDown => self.cursors[self.tab] = (at + PAGE).min(count.saturating_sub(1)),
            KeyCode::Home => self.cursors[self.tab] = 0,
            KeyCode::End => self.cursors[self.tab] = count.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(field) = fields.get(at) {
                    self.activate(field);
                }
            }
            KeyCode::Right | KeyCode::Left => {
                if let Some(field) = fields.get(at) {
                    let result = self.form.cycle(&field.pointer, key.code == KeyCode::Right);
                    self.settle(result);
                }
            }
            KeyCode::Char('n') => {
                if let Some(field) = fields.get(at) {
                    let result = self.form.set_null(&field.pointer);
                    self.settle(result);
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Esc => {
                self.status = None;
                self.error = None;
            }
            _ => {}
        }
        None
    }

    fn quit(&mut self) -> Option<Exit> {
        if self.form.is_dirty() {
            self.mode = Mode::ConfirmQuit;
            None
        } else {
            Some(Exit::Discarded)
        }
    }

    fn save(&mut self) -> Option<Exit> {
        match self.form.save() {
            Ok(saved) => Some(Exit::Saved(Box::new(saved))),
            Err(err) => {
                if let Some((tab, row)) = self.form.locate(&err.pointer) {
                    self.tab = tab;
                    self.cursors[tab] = row;
                }
                self.status = None;
                self.error = Some(err);
                None
            }
        }
    }

    fn switch_tab(&mut self, step: isize) {
        let len = self.form.tabs().len().cast_signed();
        self.tab = (self.tab.cast_signed() + step)
            .rem_euclid(len)
            .cast_unsigned();
    }

    /// Record the outcome of an edit: a refusal goes to the status bar.
    fn settle(&mut self, result: Result<(), FieldError>) {
        match result {
            Ok(()) => {
                self.status = None;
                self.error = None;
            }
            Err(err) => {
                self.status = Some(format!("{}: {}", err.pointer, err.message));
            }
        }
    }

    fn activate(&mut self, field: &Field) {
        match &field.widget {
            Widget::Group { .. } => self.form.toggle_group(&field.pointer),
            Widget::Absent { .. } => {
                let result = self.form.create(&field.pointer);
                self.settle(result);
            }
            _ if !field.editable => self.status = Some(format!("{}: read-only", field.pointer)),
            Widget::Tristate { .. } => {
                let result = self.form.cycle(&field.pointer, true);
                self.settle(result);
            }
            Widget::Picker { .. } => {
                let choices = self.form.choices(&field.pointer).unwrap_or_default();
                let index = choices.iter().position(|c| *c == field.value).unwrap_or(0);
                self.mode = Mode::Picker {
                    pointer: field.pointer.clone(),
                    choices,
                    index,
                };
            }
            Widget::Text { multiline, .. } => self.open_edit(field, *multiline),
            Widget::Integer { .. } => self.open_edit(field, false),
            Widget::Fixed => {}
        }
    }

    fn open_edit(&mut self, field: &Field, multiline: bool) {
        let text = match &field.value {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut area = TextArea::new(text.split('\n').map(str::to_owned).collect());
        area.set_cursor_line_style(Style::default());
        area.move_cursor(CursorMove::Bottom);
        area.move_cursor(CursorMove::End);
        self.mode = Mode::Edit {
            pointer: field.pointer.clone(),
            area: Box::new(area),
            multiline,
        };
    }

    fn edit(&mut self, key: KeyEvent, ctrl: bool) {
        let Mode::Edit {
            pointer,
            area,
            multiline,
        } = &mut self.mode
        else {
            return;
        };
        let accept =
            (ctrl && key.code == KeyCode::Char('s')) || (!*multiline && key.code == KeyCode::Enter);
        if accept {
            let text = area.lines().join("\n");
            let pointer = pointer.clone();
            match self.form.set_text(&pointer, &text) {
                Ok(()) => {
                    self.mode = Mode::Browse;
                    self.settle(Ok(()));
                }
                Err(err) => self.status = Some(format!("{}: {}", err.pointer, err.message)),
            }
        } else if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            self.mode = Mode::Browse;
            self.status = None;
        } else {
            area.input(key);
        }
    }

    fn pick(&mut self, key: KeyEvent, ctrl: bool) {
        let Mode::Picker {
            pointer,
            choices,
            index,
        } = &mut self.mode
        else {
            return;
        };
        match key.code {
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(choices.len().saturating_sub(1)),
            KeyCode::Enter => {
                let (pointer, value) = (pointer.clone(), choices[*index].clone());
                self.mode = Mode::Browse;
                let result = self.form.set(&pointer, value);
                self.settle(result);
            }
            KeyCode::Esc => self.mode = Mode::Browse,
            KeyCode::Char('c') if ctrl => self.mode = Mode::Browse,
            _ => {}
        }
    }

    /// Draw the whole screen.
    pub fn draw(&mut self, frame: &mut Frame) {
        let fields = self.form.fields(self.tab);
        let at = self.cursors[self.tab].min(fields.len().saturating_sub(1));
        let [tabs, body, help, status, hints] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let titles: Vec<String> = self
            .form
            .tabs()
            .iter()
            .map(|t| {
                if t.writable {
                    t.name.to_owned()
                } else {
                    format!("{} (read-only)", t.name)
                }
            })
            .collect();
        frame.render_widget(
            Tabs::new(titles)
                .select(self.tab)
                .block(Block::bordered().title(format!(" {} ", self.form.document_id())))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            tabs,
        );

        let width = fields
            .iter()
            .map(|f| 2 * usize::from(f.depth) + 4 + f.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(MAX_LABEL);
        let items: Vec<ListItem> = fields
            .iter()
            .map(|f| ListItem::new(self.row(f, width)))
            .collect();
        self.list.select((!fields.is_empty()).then_some(at));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered())
                .highlight_symbol("> ")
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            body,
            &mut self.list,
        );

        let selected = fields.get(at);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    selected.map_or(String::new(), |f| f.pointer.clone()),
                    Style::new().fg(Color::DarkGray),
                ),
                Line::raw(selected.and_then(|f| f.help.clone()).unwrap_or_default()),
            ])
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" help ")),
            help,
        );

        frame.render_widget(self.status_line(), status);
        frame.render_widget(
            Paragraph::new(self.hints()).style(Style::new().fg(Color::DarkGray)),
            hints,
        );
        self.overlay(frame);
    }

    fn row(&self, field: &Field, label_width: usize) -> Line<'static> {
        let indent = "  ".repeat(usize::from(field.depth));
        let marker = match &field.widget {
            Widget::Group { expanded: true, .. } => "[-] ",
            Widget::Group {
                expanded: false, ..
            } => "[+] ",
            Widget::Absent { creatable: true } => "[+] ",
            _ => "    ",
        };
        let value = match &field.widget {
            Widget::Group { len, .. } => format!("({len})"),
            Widget::Absent { creatable: true } => "(absent, Enter creates it)".to_owned(),
            Widget::Absent { creatable: false } => "(absent)".to_owned(),
            _ => show(&field.value),
        };
        let head = format!("{indent}{marker}{}", field.label);
        let text = format!("{head:<label_width$}  {value}");
        let failed = self
            .error
            .as_ref()
            .is_some_and(|e| e.pointer == field.pointer);
        let style = if failed {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else if field.editable
            || matches!(
                field.widget,
                Widget::Group { .. } | Widget::Absent { creatable: true }
            )
        {
            Style::new()
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Line::from(Span::styled(text, style))
    }

    fn status_line(&self) -> Paragraph<'static> {
        let state = if self.form.is_dirty() {
            "[modified]"
        } else {
            "[unchanged]"
        };
        let mut text = format!(" {state}  {} unfilled", self.form.unfilled());
        let style = if let Some(err) = &self.error {
            text.push_str(&format!("  | save refused: {}", err.message));
            Style::new().fg(Color::Red)
        } else if let Some(status) = &self.status {
            text.push_str(&format!("  | {status}"));
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        Paragraph::new(text).style(style)
    }

    fn hints(&self) -> &'static str {
        match self.mode {
            Mode::Browse => {
                " Tab section  Up/Down move  Enter edit  Left/Right cycle  n null  Ctrl-S save  q quit  ? help"
            }
            Mode::Edit { .. } => " Ctrl-S accept  Esc cancel",
            Mode::Picker { .. } => " Up/Down choose  Enter accept  Esc cancel",
            Mode::ConfirmQuit => " y discard changes  n keep editing",
            Mode::Help => " any key closes help",
        }
    }

    fn overlay(&self, frame: &mut Frame) {
        match &self.mode {
            Mode::Browse => {}
            Mode::Edit {
                pointer,
                area,
                multiline,
            } => {
                let height = if *multiline { 12 } else { 3 };
                let rect = centered(frame.area(), 70, height);
                frame.render_widget(Clear, rect);
                let mut area = area.clone();
                area.set_block(Block::bordered().title(format!(" {pointer} ")));
                frame.render_widget(&*area, rect);
            }
            Mode::Picker {
                pointer,
                choices,
                index,
            } => {
                let height = u16::try_from(choices.len() + 2).unwrap_or(u16::MAX);
                let rect = centered(frame.area(), 40, height);
                frame.render_widget(Clear, rect);
                let items: Vec<ListItem> = choices.iter().map(|c| ListItem::new(show(c))).collect();
                let mut state = ListState::default().with_selected(Some(*index));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(Block::bordered().title(format!(" {pointer} ")))
                        .highlight_symbol("> ")
                        .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
                    rect,
                    &mut state,
                );
            }
            Mode::ConfirmQuit => {
                let rect = centered(frame.area(), 44, 3);
                frame.render_widget(Clear, rect);
                frame.render_widget(
                    Paragraph::new("Discard unsaved changes? (y/n)").block(Block::bordered()),
                    rect,
                );
            }
            Mode::Help => {
                let rect = centered(frame.area(), 72, 14);
                frame.render_widget(Clear, rect);
                frame.render_widget(
                    Paragraph::new(HELP)
                        .wrap(Wrap { trim: false })
                        .block(Block::bordered().title(" help ")),
                    rect,
                );
            }
        }
    }
}

const HELP: &str = "\
Tab / Shift-Tab   switch section
Up / Down         move between fields
Enter             edit the field, open a group, create an absent block
Left / Right      step an enum or yes/no field
n                 set the field to null
Ctrl-S            check every change and save
q / Ctrl-C        quit (asks first when there are unsaved changes)

Greyed fields belong to the pipeline or to export and cannot be edited.
Run export before editing: export rewrites the install and regression
blocks and would replace what is entered here.";

fn show(value: &Value) -> String {
    match value {
        Value::Null => "(unset)".to_owned(),
        Value::String(s) if s.is_empty() => "(empty)".to_owned(),
        Value::String(s) => match s.split_once('\n') {
            Some((first, _)) => format!("{first} ..."),
            None => s.clone(),
        },
        other => other.to_string(),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
