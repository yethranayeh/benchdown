//! Jobs from a JSON file, or from the command after `--`.

use std::path::Path;

use serde_json::Value;

use crate::session::format::command_line;

/// One job the session runs after preparing the machine.
#[derive(Debug, Clone, PartialEq)]
pub struct JobSpec {
    pub name: String,
    /// The command text shown in the plan and the log.
    pub command: String,
    pub estimate_minutes: Option<f64>,
    /// Set for the `--` form, where every word is already separate.
    pub argv: Option<Vec<String>>,
}

/// Reads `[{name, command, estimateMinutes}]` from a file.
pub fn read_jobs_file(path: &Path) -> Result<Vec<JobSpec>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read jobs file {}: {error}", path.display()))?;
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("could not read jobs file {}: {error}", path.display()))?;
    let entries = parsed
        .as_array()
        .ok_or_else(|| format!("jobs file {} must contain a JSON array", path.display()))?;

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| read_job(entry, index, path))
        .collect()
}

fn read_job(value: &Value, index: usize, path: &Path) -> Result<JobSpec, String> {
    let position = format!("job {} in {}", index + 1, path.display());
    let record = value
        .as_object()
        .ok_or_else(|| format!("{position} is not an object"))?;

    let name = record
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| format!("{position} needs a name"))?;
    let command = record
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| format!("{position} needs a command"))?;
    let estimate_minutes = match record.get("estimateMinutes") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let minutes = value
                .as_f64()
                .filter(|minutes| minutes.is_finite() && *minutes >= 0.0)
                .ok_or_else(|| format!("{position} has an invalid estimateMinutes"))?;
            Some(minutes)
        }
    };

    Ok(JobSpec {
        name: name.to_owned(),
        command: command.to_owned(),
        estimate_minutes,
        argv: None,
    })
}

/// One job from `-- <command...>`, named after the command's first word.
pub fn command_job(command: &[String]) -> Result<JobSpec, String> {
    let first = command
        .first()
        .filter(|word| !word.is_empty())
        .ok_or_else(|| "the command after -- is empty".to_owned())?;
    Ok(JobSpec {
        name: first.clone(),
        command: command_line(command),
        estimate_minutes: None,
        argv: Some(command.to_vec()),
    })
}

/// The argv that runs a job: a shell for a command string, the words directly for `--`.
pub fn job_argv(job: &JobSpec) -> Vec<String> {
    match &job.argv {
        Some(argv) => argv.clone(),
        None => vec!["sh".to_owned(), "-c".to_owned(), job.command.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "benchdown-jobs-{}-{serial}-{name}",
            std::process::id()
        ))
    }

    fn write(text: &str) -> PathBuf {
        let path = temporary("jobs.json");
        std::fs::write(&path, text).expect("the jobs file writes");
        path
    }

    #[test]
    fn a_jobs_file_reads_every_job_in_order() {
        let path = write(
            r#"[{"name":"build","command":"cargo build","estimateMinutes":12.5},{"name":"test","command":"cargo test"}]"#,
        );
        let jobs = read_jobs_file(&path).expect("the jobs file reads");
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name, "build");
        assert_eq!(jobs[0].command, "cargo build");
        assert_eq!(jobs[0].estimate_minutes, Some(12.5));
        assert_eq!(jobs[1].name, "test");
        assert_eq!(jobs[1].estimate_minutes, None);
        assert!(jobs[0].argv.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_malformed_file_names_the_path() {
        let path = write("[");
        let error = read_jobs_file(&path).expect_err("the JSON does not parse");
        assert!(error.contains("jobs file"), "{error}");
        assert!(error.contains("jobs.json"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_is_not_an_array_is_refused() {
        let path = write(r#"{"name":"build"}"#);
        let error = read_jobs_file(&path).expect_err("an object is not an array");
        assert!(error.contains("JSON array"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_job_without_a_name_or_command_is_refused() {
        for text in [
            r#"[{"command":"true"}]"#,
            r#"[{"name":"","command":"true"}]"#,
            r#"[{"name":"x"}]"#,
            r#"[{"name":"x","command":"  "}]"#,
            r#"[42]"#,
        ] {
            let path = write(text);
            let error = read_jobs_file(&path).expect_err("the job is invalid");
            assert!(
                error.contains("needs a name")
                    || error.contains("needs a command")
                    || error.contains("not an object"),
                "{text}: {error}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn an_estimate_below_zero_or_not_a_number_is_refused() {
        for text in [
            r#"[{"name":"x","command":"true","estimateMinutes":-1}]"#,
            r#"[{"name":"x","command":"true","estimateMinutes":"3"}]"#,
        ] {
            let path = write(text);
            let error = read_jobs_file(&path).expect_err("the estimate is invalid");
            assert!(error.contains("invalid estimateMinutes"), "{error}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_command_after_dash_dash_is_one_job_named_after_its_first_word() {
        let job = command_job(&["echo".to_owned(), "a b".to_owned(), "$HOME".to_owned()])
            .expect("the command makes a job");
        assert_eq!(job.name, "echo");
        assert_eq!(job.command, "echo 'a b' '$HOME'");
        assert_eq!(
            job.argv,
            Some(vec![
                "echo".to_owned(),
                "a b".to_owned(),
                "$HOME".to_owned()
            ])
        );
        assert_eq!(job_argv(&job), job.argv.clone().expect("the argv is set"));
    }

    #[test]
    fn an_empty_command_after_dash_dash_is_refused() {
        assert!(command_job(&[]).is_err());
        assert!(command_job(&[String::new()]).is_err());
    }

    #[test]
    fn a_jobs_file_command_runs_through_a_shell() {
        let job = JobSpec {
            name: "x".to_owned(),
            command: "cargo test --all".to_owned(),
            estimate_minutes: None,
            argv: None,
        };
        assert_eq!(job_argv(&job), ["sh", "-c", "cargo test --all"]);
    }
}
