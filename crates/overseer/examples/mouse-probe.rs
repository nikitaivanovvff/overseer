//! Terminal mouse-reporting probe — settles "does my terminal send real
//! wheel events?" with evidence instead of guesswork.
//!
//! Run it in the terminal you're diagnosing:
//!
//! ```sh
//! cargo run --example mouse-probe
//! ```
//!
//! then wheel/two-finger-scroll over the window and read what arrives. This
//! arms the exact same capture Overseer's TUI does (raw mode +
//! `EnableMouseCapture`, i.e. xterm DECSET 1000/1002/1003 + SGR 1006), so
//! whatever you see here is byte-for-byte what Overseer sees:
//!
//! - `Mouse ScrollUp` / `Mouse ScrollDown` — your terminal sends real xterm
//!   wheel reports under capture. Overseer's wheel-over-pane scrolling works.
//! - `Key Up` / `Key Down` on wheel motion — your terminal translates wheel
//!   motion into synthetic arrow-key presses (the Terminal.app /
//!   alternate-scroll failure class; indistinguishable from real arrow
//!   keys). In Overseer, use tree focus: there Up/Down (and
//!   `Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G`) scroll the selected pane's
//!   preview, so a translated wheel still works. While a pane is *focused*,
//!   arrows forward to the agent by design — jump out (`Ctrl-h`) to scroll.
//! - Nothing at all on wheel motion — the terminal swallowed the event
//!   entirely (some terminals gate this behind a "mouse reporting" setting).
//!
//! `q`, `Esc`, or `Ctrl-C` exits and prints a per-kind tally.

use std::io::{self, Write};

use crossterm::event::{
    read, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture);
}

fn main() -> io::Result<()> {
    // Raw mode means no automatic \r on \n — print with explicit \r\n so
    // lines keep left-aligning. Deliberately *not* the alternate screen:
    // the transcript stays in your scrollback for copy/paste into an issue.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default_panic(info);
    }));

    enable_raw_mode()?;
    execute!(io::stdout(), EnableMouseCapture)?;

    let mut out = io::stdout();
    write!(
        out,
        "mouse-probe: capture armed ({} {})\r\n\
         wheel/trackpad-scroll over this window; every event prints below.\r\n\
         q / Esc / Ctrl-C to quit.\r\n\r\n",
        std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "TERM_PROGRAM unset".into()),
        std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default(),
    )?;
    out.flush()?;

    let (mut wheel, mut arrows, mut other_keys, mut other_mouse) = (0u32, 0u32, 0u32, 0u32);
    loop {
        let event = read()?;
        match &event {
            Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
                if matches!(code, KeyCode::Char('q') | KeyCode::Esc)
                    || (*code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                if matches!(code, KeyCode::Up | KeyCode::Down) {
                    arrows += 1;
                    write!(out, "Key {code:?} {modifiers:?} {kind:?}   <-- wheel-as-arrows if you were scrolling\r\n")?;
                } else {
                    other_keys += 1;
                    write!(out, "Key {code:?} {modifiers:?} {kind:?}\r\n")?;
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    wheel += 1;
                    write!(out, "Mouse {:?} at ({}, {})   <-- real wheel report\r\n", m.kind, m.column, m.row)?;
                }
                _ => {
                    other_mouse += 1;
                    write!(out, "Mouse {:?} at ({}, {})\r\n", m.kind, m.column, m.row)?;
                }
            },
            other => write!(out, "{other:?}\r\n")?,
        }
        out.flush()?;
    }

    restore();
    println!("\ntally: {wheel} real wheel reports, {arrows} Up/Down keys, {other_keys} other keys, {other_mouse} other mouse events");
    println!(
        "verdict: {}",
        if wheel > 0 {
            "this terminal sends real wheel reports — Overseer's wheel scrolling should work here."
        } else if arrows > 0 {
            "no wheel reports arrived; wheel motion came through as arrow keys — scroll from Overseer's tree focus (arrows work there), or use a terminal with real mouse reporting."
        } else {
            "no wheel-attributable events arrived — check your terminal's mouse-reporting setting."
        }
    );
    Ok(())
}
