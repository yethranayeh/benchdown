//! The restore state file, so a later run can undo a session that died.
//!
//! The file is written before the first system change and updated as changes are
//! applied. `benchdown session restore` reads it, undoes every entry in reverse
//! order and removes it when every undo succeeds.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The only state file version this build reads.
pub const STATE_VERSION: u32 = 1;

/// Points the session and `restore` at another state file, for tests and recovery.
pub const STATE_PATH_ENV: &str = "BENCHDOWN_SESSION_STATE_FILE";

/// The per-user folder under the home directory.
const APPLICATION_SUPPORT: &str = "Library/Application Support/benchdown";

const STATE_FILE_NAME: &str = "session-restore.json";

/// Which system setting one recorded change touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Spotlight,
    TimeMachine,
    Wifi,
}

/// One system change the session made, with the command that reverses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreEntry {
    pub id: String,
    pub kind: ChangeKind,
    pub label: String,
    /// The value the setting had before the session changed it.
    pub prior_state: String,
    pub undo: Vec<String>,
    /// The undo command as one line, for the log and for manual recovery.
    pub undo_command: String,
    /// Whether the session tried to apply the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_exit_code: Option<i32>,
}

/// Everything one session changed, in application order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreState {
    pub version: u32,
    pub session_id: String,
    pub created_at: String,
    pub changes: Vec<RestoreEntry>,
}

/// The fixed per-user path, or the file named by the environment for a recovery run.
pub fn resolve_state_path() -> PathBuf {
    if let Some(path) = std::env::var_os(STATE_PATH_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(APPLICATION_SUPPORT).join(STATE_FILE_NAME)
}

/// Writes the state file atomically, so a crash cannot leave half a document.
pub fn write(path: &Path, state: &RestoreState) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STATE_FILE_NAME),
        std::process::id()
    ));
    std::fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}

/// Reads and validates a state file.
pub fn read(path: &Path) -> Result<RestoreState, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let state: RestoreState = serde_json::from_str(&text).map_err(|error| format!("{error}"))?;
    if state.version != STATE_VERSION {
        return Err(format!(
            "unsupported restore state version: {}",
            state.version
        ));
    }
    if state.session_id.is_empty() || state.created_at.is_empty() {
        return Err("restore state is missing its session id or creation time".to_owned());
    }
    for (index, entry) in state.changes.iter().enumerate() {
        let position = index + 1;
        if entry.id.is_empty()
            || entry.label.is_empty()
            || entry.prior_state.is_empty()
            || entry.undo_command.is_empty()
        {
            return Err(format!("restore change {position} is missing a field"));
        }
        if entry.undo.is_empty() || entry.undo.iter().any(String::is_empty) {
            return Err(format!(
                "restore change {position} is missing its undo command"
            ));
        }
    }
    Ok(state)
}

/// Removes the state file, treating an absent file as success.
pub fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ChangeKind, undo: &[&str]) -> RestoreEntry {
        RestoreEntry {
            id: "spotlight:/".to_owned(),
            kind,
            label: "Spotlight indexing off for /".to_owned(),
            prior_state: "on".to_owned(),
            undo: undo.iter().map(|word| (*word).to_owned()).collect(),
            undo_command: undo.join(" "),
            applied: Some(true),
            applied_exit_code: Some(0),
        }
    }

    fn state() -> RestoreState {
        RestoreState {
            version: STATE_VERSION,
            session_id: "20261108-200000".to_owned(),
            created_at: "2026-11-08T20:00:00Z".to_owned(),
            changes: vec![
                entry(ChangeKind::Spotlight, &["sudo", "mdutil", "-i", "on", "/"]),
                entry(ChangeKind::TimeMachine, &["sudo", "tmutil", "enable"]),
            ],
        }
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("benchdown-state-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_state_file_round_trips() {
        let path = temporary("round-trip.json");
        let written = state();
        write(&path, &written).expect("the state writes");
        let read_back = read(&path).expect("the state reads");
        assert_eq!(read_back, written);
        assert!(
            std::fs::read_to_string(&path)
                .expect("the file is readable")
                .contains("\"time-machine\""),
            "the kind is written in kebab case"
        );
        remove(&path).expect("the state removes");
        assert!(!path.exists());
    }

    #[test]
    fn removing_twice_is_fine() {
        let path = temporary("remove-twice.json");
        remove(&path).expect("an absent file is success");
        write(&path, &state()).expect("the state writes");
        remove(&path).expect("the first removal works");
        remove(&path).expect("the second removal works");
    }

    #[test]
    fn a_version_this_build_does_not_read_is_refused() {
        let path = temporary("version.json");
        let mut written = state();
        write(&path, &written).expect("the state writes");
        written.version = 2;
        std::fs::write(
            &path,
            serde_json::to_string(&written).expect("it serializes"),
        )
        .expect("the file writes");
        let error = read(&path).expect_err("version 2 is refused");
        assert!(
            error.contains("unsupported restore state version: 2"),
            "{error}"
        );
        remove(&path).expect("the state removes");
    }

    #[test]
    fn a_change_without_an_undo_is_refused() {
        let path = temporary("no-undo.json");
        let mut written = state();
        written.changes[0].undo.clear();
        std::fs::write(
            &path,
            serde_json::to_string(&written).expect("it serializes"),
        )
        .expect("the file writes");
        let error = read(&path).expect_err("no undo is refused");
        assert!(error.contains("missing its undo command"), "{error}");
        remove(&path).expect("the state removes");
    }

    #[test]
    fn an_unknown_change_kind_is_refused() {
        let path = temporary("kind.json");
        std::fs::write(
            &path,
            r#"{"version":1,"sessionId":"a","createdAt":"b","changes":[{"id":"x","kind":"network","label":"l","priorState":"on","undo":["true"],"undoCommand":"true"}]}"#,
        )
        .expect("the file writes");
        let error = read(&path).expect_err("an unknown kind is refused");
        assert!(error.contains("unknown variant"), "{error}");
        remove(&path).expect("the state removes");
    }

    #[test]
    fn a_missing_state_file_names_the_path() {
        let path = temporary("missing.json");
        remove(&path).expect("an absent file is success");
        let error = read(&path).expect_err("a missing state file cannot be read");
        assert!(error.contains("missing.json"), "{error}");
    }
}
