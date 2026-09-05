//! Preset editor. Table of bands + preamp, live curve preview (computed locally, instant),
//! explicit apply-to-sink action, save with confirmation.
//!
//! NOTE (NOTES.md): the spec called for live-apply on every edit debounced ~100ms, but that
//! assumed a cheap runtime write. Since applying now means a full PipeWire restart (see
//! chain.rs), auto-applying on every keystroke would restart the audio system dozens of
//! times a second while dragging a value. Instead, the curve preview is instant and local
//! (no PipeWire round-trip), and pushing it to the real sink is an explicit action (`a`),
//! itself debounced so holding the key doesn't queue up repeated restarts.

use crate::application;
use crate::dsp::{band_response_db, BandType};
use crate::preset::Preset;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::io::stdout;
use std::time::{Duration, Instant};

const APPLY_DEBOUNCE: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Freq,
    Gain,
    Q,
}

pub fn run(preset: Preset) -> Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(out);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let result = event_loop(&mut terminal, preset);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

struct State {
    preset: Preset,
    row: usize, // 0 = preamp, 1.. = bands[row-1]
    field: Field,
    dirty: bool,
    show_help: bool,
    quit_confirm: bool,
    status: String,
    last_apply: Option<Instant>,
}

impl State {
    fn n_rows(&self) -> usize {
        1 + self.preset.bands.len()
    }

    fn adjust(&mut self, coarse: bool, up: bool) {
        let sign = if up { 1.0 } else { -1.0 };
        if self.row == 0 {
            let step = if coarse { 1.0 } else { 0.1 };
            self.preset.preamp_db += sign * step;
        } else {
            let band = &mut self.preset.bands[self.row - 1];
            match self.field {
                Field::Freq => {
                    let mult = if coarse { 1.2 } else { 1.05 };
                    band.freq = (if up {
                        band.freq * mult
                    } else {
                        band.freq / mult
                    })
                    .clamp(20.0, 20_000.0);
                }
                Field::Gain => {
                    let step = if coarse { 1.0 } else { 0.1 };
                    band.gain += sign * step;
                }
                Field::Q => {
                    let step = if coarse { 0.1 } else { 0.01 };
                    band.q = (band.q + sign * step).max(0.01);
                }
            }
        }
        self.dirty = true;
    }

    fn next_field(&mut self, forward: bool) {
        if self.row == 0 {
            return;
        }
        let order = [Field::Freq, Field::Gain, Field::Q];
        let idx = order.iter().position(|f| *f == self.field).unwrap_or(0);
        let next = if forward {
            (idx + 1) % order.len()
        } else {
            (idx + order.len() - 1) % order.len()
        };
        self.field = order[next];
    }
}

fn event_loop(
    terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    preset: Preset,
) -> Result<()> {
    let mut state = State {
        preset,
        row: 0,
        field: Field::Freq,
        dirty: false,
        show_help: false,
        quit_confirm: false,
        status:
            "arrows: navigate/adjust  +/-: fine  [/]: coarse  a: apply  s: save  ?: help  q: quit"
                .into(),
        last_apply: None,
    };

    loop {
        terminal.draw(|f| draw(f, &state))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        state.quit_confirm = state.quit_confirm && key.code == KeyCode::Char('q');

        match key.code {
            KeyCode::Char('?') => state.show_help = !state.show_help,
            KeyCode::Char('q') | KeyCode::Esc => {
                if state.dirty && !state.quit_confirm {
                    state.quit_confirm = true;
                    state.status =
                        "unsaved changes - press q again to discard, or s to save".into();
                } else {
                    return Ok(());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.row = state.row.checked_sub(1).unwrap_or(state.n_rows() - 1) % state.n_rows();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.row = (state.row + 1) % state.n_rows();
            }
            KeyCode::Left | KeyCode::Char('h') => state.next_field(false),
            KeyCode::Right | KeyCode::Char('l') => state.next_field(true),
            KeyCode::Char('+') | KeyCode::Char('=') => state.adjust(false, true),
            KeyCode::Char('-') | KeyCode::Char('_') => state.adjust(false, false),
            KeyCode::Char(']') => state.adjust(true, true),
            KeyCode::Char('[') => state.adjust(true, false),
            KeyCode::Char('s') => {
                crate::preset::save_preset(&state.preset)?;
                state.dirty = false;
                state.status = format!("saved {}", state.preset.name);
            }
            KeyCode::Char('a') => {
                let ready = state
                    .last_apply
                    .is_none_or(|t| t.elapsed() >= APPLY_DEBOUNCE);
                if ready {
                    state.last_apply = Some(Instant::now());
                    match application::apply_preset(&state.preset) {
                        Ok(()) => state.status = "applied to sink".into(),
                        Err(e) => state.status = format!("apply failed: {e}"),
                    }
                } else {
                    state.status = "applying too fast - hold on".into();
                }
            }
            _ => {}
        }
    }
}

fn draw(f: &mut ratatui::Frame, state: &State) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(50),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    let title = format!(
        "{}{}  preamp {:.1} dB",
        state.preset.name,
        if state.dirty { " *" } else { "" },
        state.preset.preamp_db
    );
    f.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let rows: Vec<Row> = std::iter::once(preamp_row(state))
        .chain((0..state.preset.bands.len()).map(|i| band_row(state, i)))
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["type", "freq (Hz)", "gain (dB)", "q"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("bands"));
    f.render_widget(table, chunks[1]);

    let curve_area = chunks[2];
    let width = curve_area.width.saturating_sub(2).max(10) as usize;
    let height = curve_area.height.saturating_sub(2).max(3) as usize;
    let lines = crate::render::plot(|f| response_db(&state.preset, f), width, height);
    let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("response")),
        curve_area,
    );

    let status = if state.show_help {
        "?: close help   up/down/j/k: row   left/right/h/l: field   +/-: fine   [/]: coarse   a: apply to sink   s: save   q: quit"
    } else {
        state.status.as_str()
    };
    f.render_widget(Paragraph::new(status), chunks[3]);
}

fn preamp_row(state: &State) -> Row<'static> {
    let sel = state.row == 0;
    let val = Span::styled(format!("{:.1}", state.preset.preamp_db), field_style(sel));
    Row::new(vec![
        Cell::from(row_label("preamp", sel)),
        Cell::from(val),
        Cell::from(""),
        Cell::from(""),
    ])
}

fn band_row(state: &State, i: usize) -> Row<'static> {
    let b = &state.preset.bands[i];
    let sel = state.row == i + 1;
    let kind = match b.kind {
        BandType::Peaking => "peaking",
        BandType::Lowshelf => "lowshelf",
        BandType::Highshelf => "highshelf",
    };
    Row::new(vec![
        Cell::from(row_label(kind, sel)),
        Cell::from(Span::styled(
            format!("{:.0}", b.freq),
            field_style(sel && state.field == Field::Freq),
        )),
        Cell::from(Span::styled(
            format!("{:.1}", b.gain),
            field_style(sel && state.field == Field::Gain),
        )),
        Cell::from(Span::styled(
            format!("{:.2}", b.q),
            field_style(sel && state.field == Field::Q),
        )),
    ])
}

fn row_label(text: &str, selected: bool) -> Span<'static> {
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

fn response_db(preset: &Preset, freq: f64) -> f64 {
    preset.preamp_db
        + preset
            .bands
            .iter()
            .map(|b| band_response_db(b.kind, b.freq, b.gain, b.q, freq))
            .sum::<f64>()
}
