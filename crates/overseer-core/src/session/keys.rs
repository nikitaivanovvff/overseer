use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The two terminal-mode bits `encode_key`/`encode_paste` actually consult,
/// decoupled from alacritty's own `TermMode` bitflags so nothing outside
/// `session/pty.rs` needs that crate (AGENTS.md: alacritty stays confined to
/// `session/pty.rs`). Mock mode builds this from a local `Term`'s mode
/// (`SessionManager::term_modes`); daemon mode builds it from the two bools a
/// `GridSnapshot` carries across the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermModes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
}

/// Encodes a crossterm key event into the bytes to write to a PTY, respecting
/// `mode` (application cursor keys). `None` for events with no PTY-meaningful
/// encoding (e.g. a bare modifier press). This is the one component with no
/// crate to lean on — every case here is deliberate.
pub fn encode_key(key: &KeyEvent, mode: TermModes) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let app_cursor = mode.app_cursor;

    let base: Vec<u8> = match key.code {
        KeyCode::Char(c) => encode_char(c, ctrl)?,
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
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(n) => encode_function_key(n)?.to_vec(),
        _ => return None,
    };

    // Meta/Alt sends ESC-prefixed — the common terminal convention ("meta
    // sends escape") — for keys where that's a well-defined ordinary prefix.
    // Arrow/function keys already have modifier-free CSI/SS3 forms above;
    // xterm's modifier-parameter encoding for those is out of scope here.
    let meta_prefixable =
        matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace);
    if alt && meta_prefixable {
        let mut out = vec![0x1b];
        out.extend(base);
        Some(out)
    } else {
        Some(base)
    }
}

/// Encodes pasted text, wrapping it in bracketed-paste markers when the
/// agent's terminal has that mode enabled (so e.g. Claude Code's editor
/// doesn't treat pasted newlines as individual Enter presses).
pub fn encode_paste(text: &str, mode: TermModes) -> Vec<u8> {
    if mode.bracketed_paste {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

fn encode_char(c: char, ctrl: bool) -> Option<Vec<u8>> {
    if ctrl {
        let upper = c.to_ascii_uppercase();
        if upper.is_ascii_alphabetic() {
            return Some(vec![(upper as u8) & 0x1f]);
        }
        let ctrl_byte = match c {
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '@' => 0x00,
            _ => return encode_utf8(c),
        };
        return Some(vec![ctrl_byte]);
    }
    encode_utf8(c)
}

fn encode_utf8(c: char) -> Option<Vec<u8>> {
    let mut buf = [0u8; 4];
    Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
}

fn arrow(letter: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

fn encode_function_key(n: u8) -> Option<&'static [u8]> {
    Some(match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_char_encodes_as_utf8() {
        assert_eq!(encode_key(&key(KeyCode::Char('a')), TermModes::default()), Some(b"a".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Char('é')), TermModes::default()), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn enter_esc_tab_backspace() {
        assert_eq!(encode_key(&key(KeyCode::Enter), TermModes::default()), Some(vec![b'\r']));
        assert_eq!(encode_key(&key(KeyCode::Esc), TermModes::default()), Some(vec![0x1b]));
        assert_eq!(encode_key(&key(KeyCode::Tab), TermModes::default()), Some(vec![b'\t']));
        assert_eq!(encode_key(&key(KeyCode::Backspace), TermModes::default()), Some(vec![0x7f]));
    }

    #[test]
    fn arrows_use_csi_in_normal_mode() {
        assert_eq!(encode_key(&key(KeyCode::Up), TermModes::default()), Some(vec![0x1b, b'[', b'A']));
        assert_eq!(encode_key(&key(KeyCode::Down), TermModes::default()), Some(vec![0x1b, b'[', b'B']));
        assert_eq!(encode_key(&key(KeyCode::Right), TermModes::default()), Some(vec![0x1b, b'[', b'C']));
        assert_eq!(encode_key(&key(KeyCode::Left), TermModes::default()), Some(vec![0x1b, b'[', b'D']));
    }

    #[test]
    fn arrows_use_ss3_in_application_cursor_mode() {
        let mode = TermModes { app_cursor: true, bracketed_paste: false };
        assert_eq!(encode_key(&key(KeyCode::Up), mode), Some(vec![0x1b, b'O', b'A']));
        assert_eq!(encode_key(&key(KeyCode::Down), mode), Some(vec![0x1b, b'O', b'B']));
    }

    #[test]
    fn ctrl_letter_encodes_as_control_byte() {
        // Ctrl-c must reach the agent as ETX (0x03), never be swallowed here —
        // the app layer decides whether Ctrl-c is a quit or a forward.
        assert_eq!(encode_key(&key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), TermModes::default()), Some(vec![0x03]));
        assert_eq!(encode_key(&key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL), TermModes::default()), Some(vec![0x01]));
    }

    #[test]
    fn ctrl_h_still_encodes_even_though_the_app_layer_intercepts_it() {
        // The encoder itself is dumb about Ctrl-h — interception is the caller's
        // job ("Ctrl-h is the only intercepted key" lives at the app layer).
        assert_eq!(encode_key(&key_mod(KeyCode::Char('h'), KeyModifiers::CONTROL), TermModes::default()), Some(vec![0x08]));
    }

    #[test]
    fn alt_char_gets_esc_prefix() {
        assert_eq!(
            encode_key(&key_mod(KeyCode::Char('x'), KeyModifiers::ALT), TermModes::default()),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn alt_does_not_prefix_arrows() {
        assert_eq!(
            encode_key(&key_mod(KeyCode::Up, KeyModifiers::ALT), TermModes::default()),
            Some(vec![0x1b, b'[', b'A'])
        );
    }

    #[test]
    fn unhandled_keys_return_none() {
        assert_eq!(encode_key(&key(KeyCode::CapsLock), TermModes::default()), None);
    }

    #[test]
    fn paste_wraps_in_bracketed_markers_when_mode_enabled() {
        let mode = TermModes { app_cursor: false, bracketed_paste: true };
        let bytes = encode_paste("hello\nworld", mode);
        assert_eq!(bytes, b"\x1b[200~hello\nworld\x1b[201~".to_vec());
    }

    #[test]
    fn paste_is_raw_text_when_bracketed_paste_disabled() {
        let bytes = encode_paste("hello\nworld", TermModes::default());
        assert_eq!(bytes, b"hello\nworld".to_vec());
    }
}
