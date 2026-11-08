//! Runs every `tests/scenarios/*.sc` against the real app: headless window, the real backend
//! with no account and no network (`SIGWIN_FIXTURE`), synthetic messages. A scenario passes when
//! the app exits with 0; its `expect` steps decide (see `src/headless.rs` for the steps).
//!
//! One scratch data dir per scenario, never the real one. Set `SIGWIN_SCENARIO=<name>` to run
//! one, and `SIGWIN_SCENARIO_SHOTS=<dir>` to also save a shot at the end of each.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scenarios() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scenarios");
    let only = std::env::var("SIGWIN_SCENARIO").ok();
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("tests/scenarios")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sc"))
        .filter(|p| only.as_deref().is_none_or(|o| p.file_stem().is_some_and(|s| s == o)))
        .collect();
    found.sort();
    found
}

fn run(path: &Path) -> Result<(), String> {
    let name = path.file_stem().unwrap().to_string_lossy().to_string();
    let data = std::env::temp_dir().join(format!("sigwin-scenario-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let mut script = std::fs::read_to_string(path).unwrap();
    if let Ok(dir) = std::env::var("SIGWIN_SCENARIO_SHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        script.push_str(&format!("\nshot {}\n", Path::new(&dir).join(format!("{name}.png")).display()));
    }
    let script_file = data.join("run.sc");
    std::fs::write(&script_file, script).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sigwin"))
        .env("SIGWIN_DATA", data.join("data"))
        .env("SIGWIN_FIXTURE", "1")
        .env("SIGWIN_HEADLESS", format!("@{}", script_file.display()))
        .env("SIGWIN_HEADLESS_THEME", "dark")
        .output()
        .expect("start sigwin");
    let _ = std::fs::remove_dir_all(&data);
    if out.status.success() {
        return Ok(());
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // what failed, without the log noise
    let why: Vec<&str> = text.lines().filter(|l| l.starts_with("FAILED") || l.starts_with("Error")).collect();
    Err(format!("{name}: {}", if why.is_empty() { text.lines().rev().take(8).collect::<Vec<_>>().join("\n") } else { why.join("\n") }))
}

#[test]
fn scenarios_pass() {
    let all = scenarios();
    assert!(!all.is_empty(), "no scenarios found");
    let failed: Vec<String> = all.iter().filter_map(|p| run(p).err()).collect();
    assert!(failed.is_empty(), "{} of {} scenarios failed:\n{}", failed.len(), all.len(), failed.join("\n"));
}
