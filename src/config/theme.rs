//! `[theme]` — status + chrome colors only (PHASE5B.md Task 4). Deliberately
//! small and honest: no font, no layout, nothing beyond what `ui/` already
//! centralizes as `status_style`/border styling.

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub running: Color,
    pub blocked: Color,
    pub idle: Color,
    pub done: Color,
    pub error: Color,
    pub spawning: Color,
    pub border_focused: Color,
    pub border: Color,
}

/// Reproduces today's exact pre-theme colors — asserted in a test so a
/// future change to this default can't silently drift the look everyone
/// already knows, the same guarantee `Keybindings::default()` gives for
/// keys.
impl Default for Theme {
    fn default() -> Self {
        Self {
            running: Color::Green,
            blocked: Color::Red,
            idle: Color::DarkGray,
            done: Color::Blue,
            error: Color::Red,
            spawning: Color::Cyan,
            border_focused: Color::Yellow,
            border: Color::DarkGray,
        }
    }
}

/// Parses a named `ratatui` color or `#rrggbb`. `None` for anything else —
/// callers keep the field's default and (for the top-level `[theme]`
/// deserializer) warn rather than fail the whole config.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    Some(match s.to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "dark_gray" | "dark_grey" | "darkgray" | "darkgrey" => Color::DarkGray,
        "light_red" | "lightred" => Color::LightRed,
        "light_green" | "lightgreen" => Color::LightGreen,
        "light_yellow" | "lightyellow" => Color::LightYellow,
        "light_blue" | "lightblue" => Color::LightBlue,
        "light_magenta" | "lightmagenta" => Color::LightMagenta,
        "light_cyan" | "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = std::collections::HashMap::<String, String>::deserialize(deserializer)?;
        let mut theme = Theme::default();
        for (key, value) in &raw {
            let Some(color) = parse_color(value) else {
                eprintln!("overseer: unrecognized [theme] color '{value}' for '{key}' — keeping the default");
                continue;
            };
            match key.as_str() {
                "running" => theme.running = color,
                "blocked" => theme.blocked = color,
                "idle" => theme.idle = color,
                "done" => theme.done = color,
                "error" => theme.error = color,
                "spawning" => theme.spawning = color,
                "border_focused" => theme.border_focused = color,
                "border" => theme.border = color,
                _ => eprintln!("overseer: unknown [theme] key '{key}' — ignoring"),
            }
        }
        Ok(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_reproduces_todays_exact_colors() {
        // Pins the pre-theme look: ui::status_style's Running/Blocked/
        // Idle/Done/Error/Spawning colors and the focused/unfocused border
        // colors, all as they were before [theme] existed.
        let theme = Theme::default();
        assert_eq!(theme.running, Color::Green);
        assert_eq!(theme.blocked, Color::Red);
        assert_eq!(theme.idle, Color::DarkGray);
        assert_eq!(theme.done, Color::Blue);
        assert_eq!(theme.error, Color::Red);
        assert_eq!(theme.spawning, Color::Cyan);
        assert_eq!(theme.border_focused, Color::Yellow);
        assert_eq!(theme.border, Color::DarkGray);
    }

    #[test]
    fn parse_color_named() {
        assert_eq!(parse_color("green"), Some(Color::Green));
        assert_eq!(parse_color("Dark_Gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("white"), Some(Color::White));
    }

    #[test]
    fn parse_color_hex() {
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_color("#000000"), Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn parse_color_garbage_is_none() {
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(parse_color("#fff"), None); // only 6-digit hex supported
    }

    #[test]
    fn deserialize_partial_theme_keeps_the_rest_default() {
        let raw = r#"running = "magenta""#;
        let theme: Theme = toml::from_str(raw).unwrap();
        assert_eq!(theme.running, Color::Magenta);
        assert_eq!(theme.blocked, Color::Red); // untouched default
    }

    #[test]
    fn deserialize_unrecognized_color_keeps_the_default() {
        let raw = r#"running = "not-a-color""#;
        let theme: Theme = toml::from_str(raw).unwrap();
        assert_eq!(theme.running, Color::Green); // default, warning printed
    }

    #[test]
    fn deserialize_hex_color() {
        // A single-`#` raw string (`r#"..."#`) would terminate early at the
        // `"#` inside the hex color itself — double-hash delimiters needed.
        let raw = r##"border_focused = "#123456""##;
        let theme: Theme = toml::from_str(raw).unwrap();
        assert_eq!(theme.border_focused, Color::Rgb(0x12, 0x34, 0x56));
    }
}
