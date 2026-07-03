//! Task 0 throwaway spike (PHASE6.md §4 Task 0).
//!
//! Dummy 25/75 sidebar+pane layout; spawns a real agent command in an
//! alacritty_terminal-owned PTY; renders its grid into the pane; forwards
//! keys when the pane is focused; Ctrl-h/Ctrl-l swap focus; propagates
//! resize. Throwaway: not wired into the real app, deleted after the gate.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

use crossterm::event::{
    self as ct, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Pane,
}

#[derive(Clone, Copy)]
struct GridSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
struct EventProxy {
    sender: Arc<OnceLock<EventLoopSender>>,
    dirty: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Some(sender) = self.sender.get() {
                    Notifier(sender.clone()).notify(text.into_bytes());
                }
            }
            Event::Wakeup => {
                self.dirty.store(true, Ordering::Relaxed);
            }
            Event::ChildExit(_) => {
                self.alive.store(false, Ordering::Relaxed);
                self.dirty.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

type SpikeTerm = Term<EventProxy>;

fn main() -> io::Result<()> {
    let mut cli_args = std::env::args().skip(1);
    let program = cli_args.next().unwrap_or_else(|| "claude".to_string());
    let extra_args: Vec<String> = cli_args.collect();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &program, &extra_args);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = &result {
        eprintln!("pty_spike error: {err}");
    }
    result
}

fn pane_rect(area: Rect) -> Rect {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(area);
    chunks[1]
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    program: &str,
    extra_args: &[String],
) -> io::Result<()> {
    let size = terminal.size()?;
    let full_rect = Rect::new(0, 0, size.width, size.height);
    let pane = pane_rect(full_rect);
    let grid_size = GridSize {
        cols: pane.width.max(1) as usize,
        lines: pane.height.max(1) as usize,
    };

    let dirty = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));
    let sender_slot: Arc<OnceLock<EventLoopSender>> = Arc::new(OnceLock::new());
    let proxy = EventProxy {
        sender: sender_slot.clone(),
        dirty: dirty.clone(),
        alive: alive.clone(),
    };

    let term_config = TermConfig {
        scrolling_history: 10_000,
        ..TermConfig::default()
    };
    let term = Arc::new(FairMutex::new(Term::new(term_config, &grid_size, proxy.clone())));

    let mut env = HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    let pty_options = PtyOptions {
        shell: Some(Shell::new(program.to_string(), extra_args.to_vec())),
        working_directory: std::env::current_dir().ok(),
        drain_on_exit: true,
        env,
        ..PtyOptions::default()
    };

    let window_size = WindowSize {
        num_lines: grid_size.lines as u16,
        num_cols: grid_size.cols as u16,
        cell_width: 0,
        cell_height: 0,
    };

    let pty = tty::new(&pty_options, window_size, 0)?;

    let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
    let channel = event_loop.channel();
    let _ = sender_slot.set(channel.clone());
    let mut notifier = Notifier(channel);
    let _reader_handle = event_loop.spawn(); // detached: process exits when the TUI does

    let mut focus = Focus::Tree;
    let mut last_cols = grid_size.cols;
    let mut last_lines = grid_size.lines;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
                .split(area);

            let sidebar_style = if focus == Focus::Tree {
                Style::default().add_modifier(RModifier::BOLD)
            } else {
                Style::default()
            };
            let sidebar = Paragraph::new(format!(
                "AGENTS\n\n\u{25cf} {program} (spike)\n\nFocus: {}\nCtrl-l -> pane\nq (tree-focused) quits",
                if focus == Focus::Tree { "tree" } else { "pane" }
            ))
            .style(sidebar_style)
            .block(Block::default().borders(Borders::ALL).title("overseer (spike)"));
            frame.render_widget(sidebar, chunks[0]);

            let pane_block = Block::default()
                .borders(Borders::ALL)
                .title(if focus == Focus::Pane {
                    "agent [FOCUSED — Ctrl-h to leave]"
                } else {
                    "agent"
                });
            let inner = pane_block.inner(chunks[1]);
            frame.render_widget(pane_block, chunks[1]);

            if !alive.load(Ordering::Relaxed) {
                frame.render_widget(Paragraph::new("[agent exited]"), inner);
                return;
            }

            let term_guard = term.lock();
            render_grid(&term_guard, inner, frame.buffer_mut(), focus == Focus::Pane);
        })?;
        dirty.store(false, Ordering::Relaxed);

        if !ct::poll(Duration::from_millis(100))? {
            continue;
        }

        match ct::read()? {
            CtEvent::Resize(w, h) => {
                let full_rect = Rect::new(0, 0, w, h);
                let pane = pane_rect(full_rect);
                let cols = pane.width.max(1) as usize;
                let lines = pane.height.max(1) as usize;
                if cols != last_cols || lines != last_lines {
                    last_cols = cols;
                    last_lines = lines;
                    let new_size = GridSize { cols, lines };
                    term.lock().resize(new_size);
                    notifier.on_resize(WindowSize {
                        num_lines: lines as u16,
                        num_cols: cols as u16,
                        cell_width: 0,
                        cell_height: 0,
                    });
                }
                dirty.store(true, Ordering::Relaxed);
            }
            CtEvent::Key(key) if key.kind != KeyEventKind::Release => {
                if !alive.load(Ordering::Relaxed) {
                    if key.code == KeyCode::Char('q') {
                        break;
                    }
                    continue;
                }
                match focus {
                    Focus::Tree => {
                        if key.code == KeyCode::Char('q') {
                            break;
                        } else if is_ctrl(&key, 'l') {
                            focus = Focus::Pane;
                        }
                    }
                    Focus::Pane => {
                        if is_ctrl(&key, 'h') {
                            focus = Focus::Tree;
                        } else {
                            let app_cursor = term.lock().mode().contains(TermMode::APP_CURSOR);
                            if let Some(bytes) = encode_key(&key, app_cursor) {
                                notifier.notify(bytes);
                            }
                        }
                    }
                }
                dirty.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    Ok(())
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let base: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    vec![(upper as u8) & 0x1f]
                } else {
                    let mut buf = [0u8; 4];
                    c.encode_utf8(&mut buf).as_bytes().to_vec()
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow(b'A', app_cursor),
        KeyCode::Down => arrow(b'B', app_cursor),
        KeyCode::Right => arrow(b'C', app_cursor),
        KeyCode::Left => arrow(b'D', app_cursor),
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => return None,
    };

    if alt && !base.is_empty() && !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right) {
        let mut out = vec![0x1b];
        out.extend(base);
        Some(out)
    } else {
        Some(base)
    }
}

fn arrow(letter: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

fn render_grid(term: &SpikeTerm, area: Rect, buf: &mut Buffer, show_cursor: bool) {
    let content = term.renderable_content();
    let cols = area.width as usize;
    let lines = area.height as usize;

    for cell in content.display_iter {
        let point = cell.point;
        let row = point.line.0;
        if row < 0 {
            continue;
        }
        let row = row as usize;
        let col = point.column.0;
        if row >= lines || col >= cols {
            continue;
        }

        if cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let x = area.x + col as u16;
        let y = area.y + row as u16;
        if x >= area.x + area.width || y >= area.y + area.height {
            continue;
        }

        let ch = if cell.c == '\0' { ' ' } else { cell.c };
        let fg = map_color(cell.fg);
        let bg = map_color(cell.bg);
        let mut style = Style::default().fg(fg).bg(bg);

        use alacritty_terminal::term::cell::Flags;
        if cell.flags.contains(Flags::BOLD) {
            style = style.add_modifier(RModifier::BOLD);
        }
        if cell.flags.contains(Flags::ITALIC) {
            style = style.add_modifier(RModifier::ITALIC);
        }
        if cell.flags.contains(Flags::UNDERLINE) {
            style = style.add_modifier(RModifier::UNDERLINED);
        }
        if cell.flags.contains(Flags::INVERSE) {
            style = style.add_modifier(RModifier::REVERSED);
        }

        let cell = &mut buf[(x, y)];
        cell.set_char(ch);
        cell.set_style(style);
    }

    if show_cursor {
        let cursor = content.cursor.point;
        let row = cursor.line.0;
        if row >= 0 {
            let row = row as usize;
            let col = cursor.column.0;
            if row < lines && col < cols {
                let x = area.x + col as u16;
                let y = area.y + row as u16;
                buf[(x, y)].set_style(Style::default().add_modifier(RModifier::REVERSED));
            }
        }
    }
}

fn map_color(color: alacritty_terminal::vte::ansi::Color) -> RColor {
    use alacritty_terminal::vte::ansi::{Color as AColor, NamedColor};
    match color {
        AColor::Spec(rgb) => RColor::Rgb(rgb.r, rgb.g, rgb.b),
        AColor::Named(named) => match named {
            NamedColor::Black => RColor::Black,
            NamedColor::Red => RColor::Red,
            NamedColor::Green => RColor::Green,
            NamedColor::Yellow => RColor::Yellow,
            NamedColor::Blue => RColor::Blue,
            NamedColor::Magenta => RColor::Magenta,
            NamedColor::Cyan => RColor::Cyan,
            NamedColor::White => RColor::White,
            NamedColor::BrightBlack => RColor::DarkGray,
            NamedColor::BrightRed => RColor::LightRed,
            NamedColor::BrightGreen => RColor::LightGreen,
            NamedColor::BrightYellow => RColor::LightYellow,
            NamedColor::BrightBlue => RColor::LightBlue,
            NamedColor::BrightMagenta => RColor::LightMagenta,
            NamedColor::BrightCyan => RColor::LightCyan,
            NamedColor::BrightWhite => RColor::White,
            NamedColor::Foreground => RColor::Reset,
            NamedColor::Background => RColor::Reset,
            _ => RColor::Reset,
        },
        AColor::Indexed(idx) => RColor::Indexed(idx),
    }
}
