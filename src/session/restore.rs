//! Undoing a session that died before it could restore itself.

use std::path::Path;

use crate::runner::{CaptureOptions, Captured, Runner};
use crate::session::state::{RestoreEntry, RestoreState, read, remove};

/// One undo command's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreResult {
    pub entry: RestoreEntry,
    pub exit_code: i32,
    pub stderr: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// What `benchdown session restore` needs.
pub struct RestoreOptions<'a> {
    pub runner: &'a dyn Runner,
    pub state_path: &'a Path,
    pub write: &'a dyn Fn(&str),
}

/// Undoes every entry in reverse application order, one at a time.
///
/// A failure never stops the remaining entries. `undo` reports the command's
/// result, or an error when the command could not be run at all.
pub fn restore_entries(
    entries: &[RestoreEntry],
    undo: &mut dyn FnMut(&RestoreEntry) -> Result<Captured, String>,
    before: &mut dyn FnMut(&RestoreEntry, usize, usize),
    after: &mut dyn FnMut(&RestoreResult, usize, usize),
) -> Vec<RestoreResult> {
    let mut results = Vec::new();
    let ordered: Vec<&RestoreEntry> = entries.iter().rev().collect();
    let total = ordered.len();
    for (index, entry) in ordered.into_iter().enumerate() {
        before(entry, index, total);
        let result = match undo(entry) {
            Ok(captured) => {
                let ok = captured.succeeded();
                RestoreResult {
                    entry: entry.clone(),
                    exit_code: captured.exit_code,
                    stderr: captured.stderr,
                    ok,
                    error: None,
                }
            }
            Err(error) => RestoreResult {
                entry: entry.clone(),
                exit_code: 1,
                stderr: String::new(),
                ok: false,
                error: Some(error),
            },
        };
        after(&result, index, total);
        results.push(result);
    }
    results
}

/// The `restore` command: undo a session that died before it could restore itself.
pub fn run_restore(options: RestoreOptions<'_>) -> i32 {
    let (write, runner, state_path) = (options.write, options.runner, options.state_path);
    if !state_path.exists() {
        write(&format!(
            "No restore state file at {}.\n",
            state_path.display()
        ));
        return 0;
    }

    let state = match read(state_path) {
        Ok(state) => state,
        Err(error) => {
            write(&format!("Could not read the restore state file: {error}\n"));
            write(&format!("State file kept at {}.\n", state_path.display()));
            return 1;
        }
    };

    write(&format!(
        "Restoring session {} from {}.\n",
        state.session_id,
        state_path.display()
    ));
    if state.changes.is_empty() {
        if let Err(error) = remove(state_path) {
            write(&format!(
                "Could not remove the restore state file: {error}\n"
            ));
            return 1;
        }
        write("No changes were recorded. Removed the restore state file.\n");
        return 0;
    }

    let needs_sudo = state
        .changes
        .iter()
        .any(|entry| entry.undo.first().map(String::as_str) == Some("sudo"));
    if needs_sudo {
        let sudo = ["sudo", "-v"].map(str::to_owned);
        match runner.interactive(&sudo) {
            Ok(0) => {}
            Ok(code) => {
                write(&format!(
                    "sudo was not granted (exit {code}). State file kept at {}.\n",
                    state_path.display()
                ));
                write_manual_commands(&state, write);
                return 1;
            }
            Err(error) => {
                write(&format!(
                    "sudo could not run: {error}. State file kept at {}.\n",
                    state_path.display()
                ));
                write_manual_commands(&state, write);
                return 1;
            }
        }
    }

    let mut undo = |entry: &RestoreEntry| -> Result<Captured, String> {
        runner.capture(&entry.undo, CaptureOptions::default())
    };
    let mut before = |entry: &RestoreEntry, index: usize, total: usize| {
        write(&format!(
            "[{}/{total}] {}: {}\n",
            index + 1,
            entry.label,
            entry.undo_command
        ));
    };
    let mut after = |result: &RestoreResult, index: usize, total: usize| {
        let outcome = if result.ok {
            format!("exit {}", result.exit_code)
        } else {
            format!("exit {} (failed)", result.exit_code)
        };
        write(&format!("[{}/{total}] {outcome}\n", index + 1));
        let detail = result.error.as_deref().unwrap_or(result.stderr.trim());
        if !detail.is_empty() {
            write(&format!("{detail}\n"));
        }
    };
    let results = restore_entries(&state.changes, &mut undo, &mut before, &mut after);

    let failed = results.iter().filter(|result| !result.ok).count();
    if failed > 0 {
        write(&format!(
            "{failed} of {} restore commands failed.\n",
            results.len()
        ));
        write(&format!("State file kept at {}.\n", state_path.display()));
        write_manual_commands(&state, write);
        return 1;
    }

    if let Err(error) = remove(state_path) {
        write(&format!(
            "Could not remove the restore state file: {error}\n"
        ));
        write_manual_commands(&state, write);
        return 1;
    }
    write(&format!(
        "Restored all {} changes. Removed {}.\n",
        results.len(),
        state_path.display()
    ));
    0
}

/// The undo commands for a person to run by hand, in reverse order.
pub fn write_manual_commands(state: &RestoreState, write: &dyn Fn(&str)) {
    write("Manual undo commands:\n");
    for entry in state.changes.iter().rev() {
        write(&format!("  {}\n", entry.undo_command));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Captured, FakeRunner};
    use crate::session::state::{ChangeKind, RestoreState, write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn temporary(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "benchdown-restore-{}-{serial}-{name}",
            std::process::id()
        ))
    }

    fn entry(kind: ChangeKind, label: &str, undo: &[&str]) -> RestoreEntry {
        RestoreEntry {
            id: label.to_owned(),
            kind,
            label: label.to_owned(),
            prior_state: "on".to_owned(),
            undo: undo.iter().map(|word| (*word).to_owned()).collect(),
            undo_command: undo.join(" "),
            applied: Some(true),
            applied_exit_code: Some(0),
        }
    }

    fn two_changes() -> Vec<RestoreEntry> {
        vec![
            entry(
                ChangeKind::Spotlight,
                "Spotlight /",
                &["sudo", "mdutil", "-i", "on", "/"],
            ),
            entry(
                ChangeKind::Wifi,
                "Wi-Fi en0",
                &["sudo", "networksetup", "-setairportpower", "en0", "on"],
            ),
        ]
    }

    fn state(changes: Vec<RestoreEntry>) -> RestoreState {
        RestoreState {
            version: 1,
            session_id: "20261107-120000".to_owned(),
            created_at: "2026-11-07T11:00:00Z".to_owned(),
            changes,
        }
    }

    fn write_state(path: &Path, changes: Vec<RestoreEntry>) {
        write(path, &state(changes)).expect("the state writes");
    }

    fn run(path: &Path, runner: &dyn Runner) -> (i32, String) {
        let output = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&output);
        let write = move |text: &str| {
            sink.lock()
                .expect("the output is not poisoned")
                .push_str(text)
        };
        let code = run_restore(RestoreOptions {
            runner,
            state_path: path,
            write: &write,
        });
        (
            code,
            output.lock().expect("the output is not poisoned").clone(),
        )
    }

    #[test]
    fn an_absent_state_file_is_success() {
        let path = temporary("absent.json");
        let runner = FakeRunner::always(0);
        let (code, output) = run(&path, &runner);
        assert_eq!(code, 0);
        assert!(output.contains("No restore state file"), "{output}");
    }

    #[test]
    fn every_undo_runs_in_reverse_order_and_the_file_is_removed() {
        let path = temporary("success.json");
        write_state(&path, two_changes());
        let runner = FakeRunner::always(0);
        let (code, output) = run(&path, &runner);

        assert_eq!(code, 0, "{output}");
        assert_eq!(
            runner.argv(),
            [
                vec!["sudo".to_owned(), "-v".to_owned()],
                vec![
                    "sudo".to_owned(),
                    "networksetup".to_owned(),
                    "-setairportpower".to_owned(),
                    "en0".to_owned(),
                    "on".to_owned()
                ],
                vec![
                    "sudo".to_owned(),
                    "mdutil".to_owned(),
                    "-i".to_owned(),
                    "on".to_owned(),
                    "/".to_owned()
                ],
            ]
        );
        assert!(output.contains("Restored all 2 changes"), "{output}");
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_undo_keeps_the_file_and_prints_the_manual_commands() {
        let path = temporary("failure.json");
        write_state(&path, two_changes());
        let runner = FakeRunner::new(|argv| Captured {
            exit_code: if argv.join(" ") == "sudo mdutil -i on /" {
                1
            } else {
                0
            },
            stdout: String::new(),
            stderr: "mdutil failed".to_owned(),
        });
        let (code, output) = run(&path, &runner);

        assert_eq!(code, 1);
        assert!(path.exists());
        assert!(
            output.contains("1 of 2 restore commands failed"),
            "{output}"
        );
        assert!(output.contains("mdutil failed"), "{output}");
        assert!(output.contains("Manual undo commands"), "{output}");
        assert!(
            output.contains("sudo networksetup -setairportpower en0 on"),
            "{output}"
        );
    }

    #[test]
    fn a_second_run_reports_that_there_is_nothing_left() {
        let path = temporary("twice.json");
        write_state(&path, two_changes());
        let runner = FakeRunner::always(0);
        assert_eq!(run(&path, &runner).0, 0);
        let (code, output) = run(&path, &runner);
        assert_eq!(code, 0);
        assert!(output.contains("No restore state file"), "{output}");
    }

    #[test]
    fn an_empty_change_list_only_removes_the_file() {
        let path = temporary("empty.json");
        write_state(&path, Vec::new());
        let runner = FakeRunner::always(0);
        let (code, output) = run(&path, &runner);
        assert_eq!(code, 0);
        assert!(output.contains("No changes were recorded"), "{output}");
        assert!(!path.exists());
        assert!(runner.argv().is_empty(), "no command runs");
    }

    #[test]
    fn an_unreadable_state_file_is_kept() {
        let path = temporary("broken.json");
        std::fs::write(&path, "{not json").expect("the file writes");
        let runner = FakeRunner::always(0);
        let (code, output) = run(&path, &runner);
        assert_eq!(code, 1);
        assert!(path.exists());
        assert!(
            output.contains("Could not read the restore state file"),
            "{output}"
        );
    }

    #[test]
    fn a_refused_password_keeps_the_file_and_prints_what_to_run() {
        let path = temporary("sudo.json");
        write_state(&path, two_changes());
        let runner = FakeRunner::always(0).with_interactive_exit(1);
        let (code, output) = run(&path, &runner);
        assert_eq!(code, 1);
        assert!(path.exists());
        assert!(output.contains("sudo was not granted (exit 1)"), "{output}");
        assert!(output.contains("sudo mdutil -i on /"), "{output}");
        assert_eq!(
            runner.argv(),
            [vec!["sudo".to_owned(), "-v".to_owned()]],
            "no undo runs without the password"
        );
    }
}
