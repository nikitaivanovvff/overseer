use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, time::Duration};

mod agent;
mod app;
mod session;
mod ui;

use app::App;

fn main() -> Result<()> {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_panic(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::with_mock_data();
    let res = run_app(&mut terminal, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();

    res
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        // Poll with a 100ms timeout so the spinner and future IPC updates can
        // redraw without waiting for a keypress.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => break,
                    KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => break,
                    KeyCode::Char('j') | KeyCode::Down
                        if key.modifiers == KeyModifiers::NONE =>
                    {
                        app.agent_tree.move_down();
                    }
                    KeyCode::Char('k') | KeyCode::Up
                        if key.modifiers == KeyModifiers::NONE =>
                    {
                        app.agent_tree.move_up();
                    }
                    // Space collapses/expands a root agent's children list.
                    KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                        app.agent_tree.toggle_expand();
                    }
                    // Enter / o — focus the selected agent's pane.
                    // F2 — toggle focus back to tree from anywhere.
                    KeyCode::Enter | KeyCode::Char('o')
                        if key.modifiers == KeyModifiers::NONE =>
                    {
                        app.toggle_focus();
                    }
                    KeyCode::F(2) => {
                        app.toggle_focus();
                    }
                    _ => {}
                }
            }
        }

        app.tick();
    }
    Ok(())
}
