//! Two screens: a file tree of PEQ presets, and an EQ view (braille response curve on
//! top, editable band table below). Edits change only an in-memory draft that is
//! previewed live through EasyEffects; the file on disk changes only on save.

use crate::dsp::{preset_response_db, BandType, FS};
use crate::easyeffects;
use crate::library::Tree;
use crate::preset::{load_peq_file, to_peq, write_atomic, Band, Preset};
use crate::validation as v;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn run(root: PathBuf) -> Result<()> {
    let _session = TerminalSession::enter(CrosstermTerminalOps)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    App::new(root).event_loop(&mut terminal)
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Tree,
    Eq,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Freq,
    Gain,
    Q,
}

/// What to do once an unsaved draft has been saved or discarded.
#[derive(Clone, PartialEq)]
enum After {
    Open(PathBuf),
    Quit,
}

struct Open {
    path: PathBuf,
    saved: Preset,
    draft: Preset,
}

struct App {
    screen: Screen,
    tree: Tree,
    open: Option<Open>,
    row: usize, // 0 = preamp, 1.. = bands[row-1]
    field: Field,
    preview_due: Option<Instant>,
    confirm: Option<After>,
    show_help: bool,
    bypassed: bool,
    status: String,
}

impl App {
    fn new(root: PathBuf) -> Self {
        Self {
            screen: Screen::Tree,
            tree: Tree::new(root),
            open: None,
            row: 0,
            field: Field::Gain,
            preview_due: None,
            confirm: None,
            show_help: false,
            bypassed: easyeffects::bypassed().unwrap_or(false),
            status: "enter: open   b: bypass   tab: switch screen   ?: help   q: quit".into(),
        }
    }

    fn dirty(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.draft != o.saved)
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) -> Result<()> {
        loop {
            if self.preview_due.is_some_and(|t| Instant::now() >= t) {
                self.preview_due = None;
                if let Some(o) = &self.open {
                    self.report(easyeffects::load(&o.draft), "previewing (unsaved)");
                }
            }
            terminal.draw(|f| self.draw(f))?;

            let timeout = if self.preview_due.is_some() { 30 } else { 250 };
            if !event::poll(Duration::from_millis(timeout))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);

            if let Some(after) = self.confirm.take() {
                match key.code {
                    KeyCode::Char('s') => {
                        if !self.save() {
                            continue;
                        }
                    }
                    KeyCode::Char('d') => self.discard(),
                    _ => {
                        self.status = "cancelled".into();
                        continue;
                    }
                }
                match after {
                    After::Quit => return Ok(()),
                    After::Open(path) => self.open_file(path),
                }
                continue;
            }

            if ctrl_c || key.code == KeyCode::Char('q') {
                if self.leave(After::Quit) {
                    return Ok(());
                }
                continue;
            }
            match key.code {
                KeyCode::Char('?') => self.show_help = !self.show_help,
                KeyCode::Char('b') => {
                    let on = !self.bypassed;
                    let result = easyeffects::set_bypass(on);
                    if result.is_ok() {
                        self.bypassed = on;
                    }
                    self.report(result, if on { "bypass ON" } else { "bypass off" });
                }
                KeyCode::Tab => {
                    self.screen = match self.screen {
                        Screen::Tree if self.open.is_some() => Screen::Eq,
                        _ => Screen::Tree,
                    }
                }
                _ => match self.screen {
                    Screen::Tree => self.tree_key(key.code),
                    Screen::Eq => self.eq_key(key.code),
                },
            }
        }
    }

    /// Ask before leaving a dirty draft; returns true when it is fine to proceed now.
    fn leave(&mut self, after: After) -> bool {
        if !self.dirty() {
            return true;
        }
        self.confirm = Some(after);
        self.status = "unsaved changes - s: save, d: discard, other key: cancel".into();
        false
    }

    fn report(&mut self, result: Result<()>, ok: &str) {
        self.status = match result {
            Ok(()) => ok.to_string(),
            Err(e) => format!("error: {e:#}"),
        };
    }

    fn open_file(&mut self, path: PathBuf) {
        match load_peq_file(&path) {
            Ok(preset) => {
                let result = easyeffects::load(&preset);
                self.report(result, &format!("loaded {}", preset.name));
                self.open = Some(Open {
                    path,
                    saved: preset.clone(),
                    draft: preset,
                });
                self.row = 0;
                self.preview_due = None;
                self.screen = Screen::Eq;
            }
            Err(e) => self.status = format!("cannot open: {e:#}"),
        }
    }

    fn save(&mut self) -> bool {
        let Some(o) = &mut self.open else {
            return false;
        };
        match write_atomic(&o.path, &to_peq(&o.draft)) {
            Ok(()) => {
                o.saved = o.draft.clone();
                self.status = format!("saved {}", o.path.display());
                self.tree.refresh();
                true
            }
            Err(e) => {
                self.status = format!("save failed: {e:#}");
                false
            }
        }
    }

    /// Drop the draft and put the saved curve back into EasyEffects.
    fn discard(&mut self) {
        if let Some(o) = &mut self.open {
            o.draft = o.saved.clone();
            self.preview_due = None;
            let result = easyeffects::load(&o.saved);
            self.report(result, "discarded changes");
            self.row = self
                .row
                .min(self.open.as_ref().map_or(0, |o| o.draft.bands.len()));
        }
    }

    fn tree_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.tree.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.tree.move_by(1),
            KeyCode::PageUp => self.tree.move_by(-10),
            KeyCode::PageDown => self.tree.move_by(10),
            KeyCode::Left | KeyCode::Char('h') => self.tree.set_expanded(false),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                let Some(node) = self.tree.selected().cloned() else {
                    return;
                };
                if node.is_dir {
                    let open = self.tree.expanded.contains(&node.path);
                    self.tree.set_expanded(!open || code != KeyCode::Enter);
                } else if self.open.as_ref().is_some_and(|o| o.path == node.path) {
                    self.screen = Screen::Eq;
                } else if self.leave(After::Open(node.path.clone())) {
                    self.open_file(node.path);
                }
            }
            _ => {}
        }
    }

    fn eq_key(&mut self, code: KeyCode) {
        let Some(o) = &mut self.open else {
            return;
        };
        let rows = 1 + o.draft.bands.len();
        let before = o.draft.clone();
        match code {
            KeyCode::Esc => self.screen = Screen::Tree,
            KeyCode::Up | KeyCode::Char('k') => self.row = (self.row + rows - 1) % rows,
            KeyCode::Down | KeyCode::Char('j') => self.row = (self.row + 1) % rows,
            KeyCode::Left | KeyCode::Char('h') => self.next_field(false),
            KeyCode::Right | KeyCode::Char('l') => self.next_field(true),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust(false, true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.adjust(false, false),
            KeyCode::Char(']') => self.adjust(true, true),
            KeyCode::Char('[') => self.adjust(true, false),
            KeyCode::Char('t') if self.row > 0 => {
                let b = &mut o.draft.bands[self.row - 1];
                b.kind = match b.kind {
                    BandType::Peaking => BandType::Lowshelf,
                    BandType::Lowshelf => BandType::Highshelf,
                    BandType::Highshelf => BandType::Peaking,
                };
            }
            KeyCode::Char(' ') if self.row > 0 => {
                let b = &mut o.draft.bands[self.row - 1];
                b.enabled = !b.enabled;
            }
            KeyCode::Char('n') if o.draft.bands.len() < v::MAX_BANDS => {
                let at = self.row; // insert after the selected row
                o.draft.bands.insert(
                    at,
                    Band {
                        kind: BandType::Peaking,
                        freq: 1000.0,
                        gain: 0.0,
                        q: 1.0,
                        enabled: true,
                    },
                );
                self.row = at + 1;
            }
            KeyCode::Char('x') if self.row > 0 => {
                o.draft.bands.remove(self.row - 1);
                self.row -= 1;
            }
            KeyCode::Char('s') => {
                self.save();
            }
            KeyCode::Char('u') => self.discard(),
            _ => {}
        }
        if self.open.as_ref().is_some_and(|o| o.draft != before) {
            self.preview_due = Some(Instant::now() + PREVIEW_DEBOUNCE);
        }
    }

    fn next_field(&mut self, forward: bool) {
        let order = [Field::Freq, Field::Gain, Field::Q];
        let idx = order.iter().position(|f| *f == self.field).unwrap_or(0);
        let step = if forward { 1 } else { order.len() - 1 };
        self.field = order[(idx + step) % order.len()];
    }

    fn adjust(&mut self, coarse: bool, up: bool) {
        let Some(o) = &mut self.open else {
            return;
        };
        let sign = if up { 1.0 } else { -1.0 };
        if self.row == 0 {
            let step = if coarse { 1.0 } else { 0.1 };
            o.draft.preamp_db = round_to(o.draft.preamp_db + sign * step, 0.1)
                .clamp(v::MIN_PREAMP_DB, v::MAX_PREAMP_DB);
            return;
        }
        let band = &mut o.draft.bands[self.row - 1];
        match self.field {
            Field::Freq => {
                let mult: f64 = if coarse { 1.2 } else { 1.02 };
                let f = band.freq * if up { mult } else { 1.0 / mult };
                band.freq = f.round().clamp(v::MIN_FREQUENCY_HZ, v::MAX_FREQUENCY_HZ);
            }
            Field::Gain => {
                let step = if coarse { 1.0 } else { 0.1 };
                band.gain =
                    round_to(band.gain + sign * step, 0.1).clamp(v::MIN_GAIN_DB, v::MAX_GAIN_DB);
            }
            Field::Q => {
                let step = if coarse { 0.1 } else { 0.01 };
                band.q = round_to(band.q + sign * step, 0.001).clamp(v::MIN_Q, v::MAX_Q);
            }
        }
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(1)])
            .split(f.area());
        match (self.screen, &self.open) {
            (Screen::Eq, Some(o)) => self.draw_eq(f, o, chunks[0]),
            _ => self.draw_tree(f, chunks[0]),
        }
        let status = if self.show_help {
            match self.screen {
                Screen::Tree => "j/k: move  l/enter: open/expand  h: collapse  b: bypass  tab: EQ screen  q: quit",
                Screen::Eq => "j/k: row  h/l: field  +/-: fine  [/]: coarse  t: type  space: on/off  n: add  x: delete  b: bypass  s: save  u: discard  esc/tab: tree  q: quit",
            }
        } else {
            self.status.as_str()
        };
        let mut line = vec![Span::raw(status)];
        if self.bypassed {
            line.insert(
                0,
                Span::styled(
                    " BYPASS ",
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                ),
            );
            line.insert(1, Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(line)), chunks[1]);
    }

    fn draw_tree(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let open_path = self.open.as_ref().map(|o| &o.path);
        let items: Vec<ListItem> = self
            .tree
            .visible
            .iter()
            .map(|n| {
                let name = n.path.file_name().unwrap_or_default().to_string_lossy();
                let icon = match (n.is_dir, self.tree.expanded.contains(&n.path)) {
                    (true, true) => "▾ ",
                    (true, false) => "▸ ",
                    _ => "  ",
                };
                let mut style = Style::default();
                if n.is_dir {
                    style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                } else if !n.ok {
                    style = style.fg(Color::DarkGray);
                }
                let mut spans = vec![
                    Span::raw("  ".repeat(n.depth)),
                    Span::styled(format!("{icon}{name}"), style),
                ];
                if !n.ok {
                    spans.push(Span::styled(" !", Style::default().fg(Color::Yellow)));
                }
                if open_path == Some(&n.path) {
                    let mark = if self.dirty() { " ● *" } else { " ●" };
                    spans.push(Span::styled(mark, Style::default().fg(Color::Green)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", self.tree.root.display())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(self.tree.cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_eq(&self, f: &mut ratatui::Frame, o: &Open, area: ratatui::layout::Rect) {
        let table_height = (o.draft.bands.len() as u16 + 4).min(area.height / 2);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(table_height)])
            .split(area);

        let title = format!(
            " {}{}  preamp {:.1} dB ",
            o.draft.name,
            if self.dirty() { " * (unsaved)" } else { "" },
            o.draft.preamp_db
        );
        let curve = chunks[0];
        let width = curve.width.saturating_sub(2).max(10) as usize;
        let height = curve.height.saturating_sub(2).max(3) as usize;
        let lines = crate::render::plot(|hz| preset_response_db(&o.draft, hz, FS), width, height);
        let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
        f.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(Color::Cyan))
                .block(Block::default().borders(Borders::ALL).title(title)),
            curve,
        );

        let rows: Vec<Row> = std::iter::once(self.preamp_row(o))
            .chain((0..o.draft.bands.len()).map(|i| self.band_row(o, i)))
            .collect();
        let selected = self.row;
        let table = Table::new(
            rows,
            [
                Constraint::Length(4),
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(["on", "type", "freq (Hz)", "gain (dB)", "q"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" bands "));
        let mut state = ratatui::widgets::TableState::default().with_selected(Some(selected));
        f.render_stateful_widget(table, chunks[1], &mut state);
    }

    fn preamp_row(&self, o: &Open) -> Row<'static> {
        let sel = self.row == 0;
        Row::new(vec![
            Cell::from(""),
            Cell::from(label("preamp", sel)),
            Cell::from(""),
            Cell::from(Span::styled(
                format!("{:.1}", o.draft.preamp_db),
                field_style(sel),
            )),
            Cell::from(""),
        ])
    }

    fn band_row(&self, o: &Open, i: usize) -> Row<'static> {
        let b = &o.draft.bands[i];
        let sel = self.row == i + 1;
        let kind = match b.kind {
            BandType::Peaking => "peaking",
            BandType::Lowshelf => "lowshelf",
            BandType::Highshelf => "highshelf",
        };
        let row = Row::new(vec![
            Cell::from(if b.enabled { "[x]" } else { "[ ]" }),
            Cell::from(label(kind, sel)),
            Cell::from(Span::styled(
                format!("{:.0}", b.freq),
                field_style(sel && self.field == Field::Freq),
            )),
            Cell::from(Span::styled(
                format!("{:.1}", b.gain),
                field_style(sel && self.field == Field::Gain),
            )),
            Cell::from(Span::styled(
                format!("{:.2}", b.q),
                field_style(sel && self.field == Field::Q),
            )),
        ]);
        if b.enabled {
            row
        } else {
            row.style(Style::default().fg(Color::DarkGray))
        }
    }
}

fn round_to(x: f64, step: f64) -> f64 {
    (x / step).round() * step
}

fn label(text: &str, selected: bool) -> Span<'static> {
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    Span::styled(text.to_string(), style)
}

fn field_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::default()
    }
}

trait TerminalOps {
    fn enable_raw(&mut self) -> Result<()>;
    fn enter_alternate(&mut self) -> Result<()>;
    fn leave_alternate(&mut self) -> Result<()>;
    fn disable_raw(&mut self) -> Result<()>;
}

struct CrosstermTerminalOps;

impl TerminalOps for CrosstermTerminalOps {
    fn enable_raw(&mut self) -> Result<()> {
        enable_raw_mode().map_err(Into::into)
    }

    fn enter_alternate(&mut self) -> Result<()> {
        execute!(stdout(), EnterAlternateScreen).map_err(Into::into)
    }

    fn leave_alternate(&mut self) -> Result<()> {
        execute!(stdout(), LeaveAlternateScreen).map_err(Into::into)
    }

    fn disable_raw(&mut self) -> Result<()> {
        disable_raw_mode().map_err(Into::into)
    }
}

/// Restores terminal modes on normal return, error and panic unwinding.
struct TerminalSession<T: TerminalOps> {
    ops: T,
    raw: bool,
    alternate: bool,
}

impl<T: TerminalOps> TerminalSession<T> {
    fn enter(mut ops: T) -> Result<Self> {
        ops.enable_raw()?;
        if let Err(error) = ops.enter_alternate() {
            let _ = ops.disable_raw();
            return Err(error);
        }
        Ok(Self {
            ops,
            raw: true,
            alternate: true,
        })
    }
}

impl<T: TerminalOps> Drop for TerminalSession<T> {
    fn drop(&mut self) {
        if self.alternate {
            let _ = self.ops.leave_alternate();
            self.alternate = false;
        }
        if self.raw {
            let _ = self.ops.disable_raw();
            self.raw = false;
        }
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::{TerminalOps, TerminalSession};
    use anyhow::{bail, Result};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeOps {
        calls: Rc<RefCell<Vec<&'static str>>>,
        fail_alternate: bool,
    }

    impl TerminalOps for FakeOps {
        fn enable_raw(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("enable_raw");
            Ok(())
        }

        fn enter_alternate(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("enter_alternate");
            if self.fail_alternate {
                bail!("alternate screen failed")
            }
            Ok(())
        }

        fn leave_alternate(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("leave_alternate");
            Ok(())
        }

        fn disable_raw(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("disable_raw");
            Ok(())
        }
    }

    fn fake(fail_alternate: bool) -> (FakeOps, Rc<RefCell<Vec<&'static str>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        (
            FakeOps {
                calls: calls.clone(),
                fail_alternate,
            },
            calls,
        )
    }

    #[test]
    fn session_restores_terminal_on_drop() {
        let (ops, calls) = fake(false);
        drop(TerminalSession::enter(ops).unwrap());
        assert_eq!(
            *calls.borrow(),
            [
                "enable_raw",
                "enter_alternate",
                "leave_alternate",
                "disable_raw"
            ]
        );
    }

    #[test]
    fn partial_entry_rolls_back_raw_mode() {
        let (ops, calls) = fake(true);
        assert!(TerminalSession::enter(ops).is_err());
        assert_eq!(
            *calls.borrow(),
            ["enable_raw", "enter_alternate", "disable_raw"]
        );
    }

    #[test]
    fn panic_unwinding_restores_terminal() {
        let (ops, calls) = fake(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = TerminalSession::enter(ops).unwrap();
            panic!("test panic");
        }));
        assert!(result.is_err());
        assert_eq!(calls.borrow().last(), Some(&"disable_raw"));
    }
}
