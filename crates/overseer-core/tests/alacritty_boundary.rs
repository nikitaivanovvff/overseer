//! Guard test for the terminal-backend seal: `alacritty_terminal` may only
//! ever be imported from `src/session/pty.rs` in this crate (`overseer-core`)
//! — every other file talks to `SessionManager` through backend-neutral
//! types (`GridSnapshot`, `session::keys::TermModes`). A plain source grep
//! rather than a compile-time check because the point is to catch a *future*
//! regression (a new `use alacritty_terminal::...` creeping in elsewhere),
//! not to re-verify today's code compiles. The `overseer` bin crate has its
//! own copy of this guard (`crates/overseer/tests/alacritty_boundary.rs`)
//! asserting zero occurrences there at all, now that the terminal backend
//! lives only in this crate.

use std::path::{Path, PathBuf};

#[test]
fn alacritty_terminal_is_only_imported_in_session_pty() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed = root.join("session").join("pty.rs");
    let mut offenders = Vec::new();
    visit(&root, &allowed, &mut offenders);

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
