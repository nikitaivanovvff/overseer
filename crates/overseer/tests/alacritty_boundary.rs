//! Guard test, `overseer` bin crate's half: the terminal-emulator crate
//! (alacritty's, confined by AGENTS.md's house rule to `overseer-core`'s
//! `src/session/pty.rs`) must never appear anywhere in this crate's source at
//! all — this crate consumes only the backend-neutral `GridSnapshot`/
//! `TermModes` DTOs. The core crate's own half of the guard lives at
//! `crates/overseer-core/tests/alacritty_boundary.rs` and carves out the one
//! allowed file; this crate has no exception to carve out. The needle is
//! assembled at runtime so this test file itself stays clean under a plain
//! source grep of `crates/overseer/`.

use std::path::{Path, PathBuf};

#[test]
fn terminal_backend_crate_is_never_imported_in_the_bin_crate() {
    let needle = format!("alacritty{}terminal", '_');
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    visit(&root, &needle, &mut offenders);

    assert!(
        offenders.is_empty(),
        "terminal-emulator crate referenced in the overseer bin crate (must live only in \
         overseer-core's session/pty.rs): {offenders:?}"
    );
}

fn visit(dir: &Path, needle: &str, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            visit(&path, needle, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let contents = std::fs::read_to_string(&path).expect("read source file");
        if contents.contains(needle) {
            offenders.push(path.display().to_string());
        }
    }
}
