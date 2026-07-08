//! Guard test for the terminal-backend seal: `alacritty_terminal` may only
//! ever be imported from `src/session/pty.rs` — every other file talks to
//! `SessionManager` through backend-neutral types (`GridSnapshot`,
//! `session::keys::TermModes`). A plain source grep rather than a
//! compile-time check because the point is to catch a *future* regression
//! (a new `use alacritty_terminal::...` creeping into `ui/` or elsewhere),
//! not to re-verify today's code compiles.

use std::path::Path;

#[test]
fn alacritty_terminal_is_only_imported_in_session_pty() {
    let allowed = Path::new("src/session/pty.rs");
    let mut offenders = Vec::new();
    visit(Path::new("src"), allowed, &mut offenders);

    assert!(
        offenders.is_empty(),
        "alacritty_terminal referenced outside src/session/pty.rs: {offenders:?}"
    );
}

fn visit(dir: &Path, allowed: &Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            visit(&path, allowed, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path == allowed {
            continue;
        }
        let contents = std::fs::read_to_string(&path).expect("read source file");
        if contents.contains("alacritty_terminal") {
            offenders.push(path.display().to_string());
        }
    }
}
