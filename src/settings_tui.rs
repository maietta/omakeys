//! Interactive terminal UI for the settings/cheat-sheet menu, launched in a real terminal
//! window (via `omarchy-launch-terminal`) when Super is held past the long-press threshold --
//! see `main.rs`'s `Commands::Menu` handler and `scripts/super-hold-start.sh`. Replaces the
//! Cairo-drawn overlay panel this used to be: a real TUI instead of a GTK layer-shell surface,
//! since the settings menu doesn't need to sit on top of other windows or track the mouse the
//! way grid/hint mode do.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};

use crate::config::{self, Settings, NUDGE_STEP_INCREMENT, NUDGE_STEP_MAX, NUDGE_STEP_MIN};

/// Keybind reference shown in the TUI, as (key, description) pairs -- same content the old
/// Cairo-drawn menu showed.
const CHEAT_SHEET: &[(&str, &str)] = &[
    ("Right Shift / Super (tap)", "Open grid mode"),
    ("Control_R", "Open hint mode (AT-SPI/vision element hints)"),
    ("Super (hold)", "Open this menu"),
    ("<coarse><fine>", "Grid: pick a cell by its two-key code"),
    ("h j k l / arrows", "Nudge the cursor"),
    ("Space", "Click"),
    ("Shift + Space", "Right-click"),
    ("Left Shift (hold)", "Click-and-drag: hold, nudge to select, release to finish"),
    ("<label>", "Hints: type a target's label to warp + click it"),
    ("Backspace", "Undo the last typed character"),
    ("Escape", "Close the overlay"),
];

/// Run the settings TUI, blocking until the user quits (Esc or q). Reads/writes
/// `Settings::nudge_step` live via `config::load`/`config::save`, same as the old menu did --
/// takes over the whole terminal (raw mode + alternate screen), restored on exit even if the
/// event loop returns an error.
pub fn run() -> anyhow::Result<()> {
    let mut settings = config::load();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut settings);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, settings: &mut Settings) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, settings))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let delta = match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('+') | KeyCode::Char('=') => NUDGE_STEP_INCREMENT,
                    KeyCode::Char('-') => -NUDGE_STEP_INCREMENT,
                    _ => 0.0,
                };
                if delta != 0.0 {
                    settings.nudge_step = (settings.nudge_step + delta).clamp(NUDGE_STEP_MIN, NUDGE_STEP_MAX);
                    if let Err(e) = config::save(settings) {
                        log::warn!("omakeys: failed to save settings: {e}");
                    }
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, settings: &Settings) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(CHEAT_SHEET.len() as u16 + 2),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let (title_area, sheet_area, setting_area, footer_area, credit_area) =
        (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4]);

    frame.render_widget(
        Paragraph::new("OmaKeys")
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL)),
        title_area,
    );

    let key_col_w = CHEAT_SHEET.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let items: Vec<ListItem> = CHEAT_SHEET
        .iter()
        .map(|(key, desc)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{key:<key_col_w$}"),
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(*desc),
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Keybindings")),
        sheet_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("+ / -", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!("  Nudge speed: {:.0}px per press", settings.nudge_step)),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Settings")),
        setting_area,
    );

    frame.render_widget(
        Paragraph::new("Esc / q to quit").style(Style::default().fg(Color::DarkGray)).alignment(Alignment::Center),
        footer_area,
    );

    frame.render_widget(
        Paragraph::new("Nick Maietta <nick@maietta.org>")
            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))
            .alignment(Alignment::Center),
        credit_area,
    );
}
