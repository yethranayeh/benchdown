//! The restore guard: a small root shell that applies changes and their undos,
//! and restores by itself if the session disappears.

use std::path::Path;

use crate::runner::{Captured, PipeProcess, Runner};

/// The guard script, embedded so an installed benchdown carries it.
pub const SCRIPT: &str = include_str!("guard.sh");

/// The first line the guard prints, before it reads requests.
pub const READY_LINE: &str = "ready";

/// Where the guard writes its log and its final restore report.
pub struct GuardPaths<'a> {
    pub guard_log: &'a Path,
    pub state: &'a Path,
    pub status: &'a Path,
    pub out_status: &'a Path,
}

/// Writes the guard script into the session folder, executable by its owner.
pub fn write_script(path: &Path) -> Result<(), String> {
    std::fs::write(path, SCRIPT).map_err(|error| format!("{}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

/// Parses one `<exit-code> <stderr>` reply line.
pub fn parse_reply(line: &str) -> Captured {
    let (exit_text, stderr) = line.split_once(' ').unwrap_or((line, ""));
    match exit_text.parse::<i32>() {
        Ok(exit_code) => Captured {
            exit_code,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        },
        Err(_) => Captured {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("unexpected guard reply: {line}"),
        },
    }
}

/// A running guard, with one request/reply exchange per line.
pub struct Guard {
    process: Box<dyn PipeProcess>,
    closed: bool,
}

impl Guard {
    /// Starts the guard through the primed sudo ticket.
    pub fn start(
        runner: &dyn Runner,
        script: &Path,
        paths: &GuardPaths<'_>,
        cwd: Option<&Path>,
    ) -> Result<Self, String> {
        let argv: Vec<String> = [
            "sudo".to_owned(),
            "-n".to_owned(),
            "/bin/sh".to_owned(),
            script.display().to_string(),
            paths.guard_log.display().to_string(),
            paths.state.display().to_string(),
            paths.status.display().to_string(),
            paths.out_status.display().to_string(),
        ]
        .to_vec();
        let mut process = runner.pipe(&argv, cwd)?;
        let first = match process.read_line() {
            Ok(line) => line,
            Err(error) => {
                let detail = match process.close() {
                    Ok(closed) => format!(
                        " (exit {}){}",
                        closed.exit_code,
                        stderr_detail(&closed.stderr)
                    ),
                    Err(_) => String::new(),
                };
                return Err(format!("the restore guard did not start: {error}{detail}"));
            }
        };
        if first != READY_LINE {
            let _ = process.close();
            return Err(format!(
                "the restore guard did not start: unexpected first line {first:?}"
            ));
        }
        Ok(Self {
            process,
            closed: false,
        })
    }

    /// Sends one request and returns the guard's reply.
    pub fn request(&mut self, request: &str) -> Result<Captured, String> {
        let outcome = self
            .process
            .write_line(request)
            .and_then(|()| self.process.read_line())
            .map(|line| parse_reply(&line));
        if outcome.is_err() {
            let _ = self.close();
        }
        outcome
    }

    /// Sends END, so the guard exits without restoring, and waits for it.
    pub fn finish(&mut self) -> Result<Captured, String> {
        let written = self.process.write_line("END");
        let closed = self.close();
        match (written, closed) {
            (Ok(()), Ok(closed)) => Ok(closed),
            (Err(error), _) => Err(format!("could not send END to the restore guard: {error}")),
            (Ok(()), Err(error)) => Err(error),
        }
    }

    /// Closes the pipe. If END was not sent, the guard restores by itself.
    pub fn close(&mut self) -> Result<Captured, String> {
        if self.closed {
            return Ok(Captured {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        self.closed = true;
        self.process.close()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Dropping the pipe closes the guard's stdin, which makes it restore.
        let _ = self.close();
    }
}

fn stderr_detail(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::session::fakes::FakeRunner;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::Arc;

    fn temporary(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "benchdown-guard-{}-{serial}-{name}",
            std::process::id()
        ))
    }

    fn script_path() -> PathBuf {
        let path = temporary("guard.sh");
        write_script(&path).expect("the guard script writes");
        path
    }

    fn paths(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        (
            root.join("guard.log"),
            root.join("state.json"),
            root.join("status.txt"),
            root.join("out-status.txt"),
        )
    }

    #[test]
    fn a_reply_carries_its_exit_code_and_flattened_stderr() {
        assert_eq!(
            parse_reply("0 "),
            Captured {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new()
            }
        );
        assert_eq!(
            parse_reply("2 rejected request"),
            Captured {
                exit_code: 2,
                stdout: String::new(),
                stderr: "rejected request".to_owned()
            }
        );
        assert_eq!(
            parse_reply("0"),
            Captured {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new()
            }
        );
        let unexpected = parse_reply("what");
        assert_eq!(unexpected.exit_code, 1);
        assert!(
            unexpected.stderr.contains("unexpected guard reply"),
            "{unexpected:?}"
        );
    }

    #[test]
    fn the_script_is_written_executable() {
        let path = script_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("the script exists")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        assert!(
            std::fs::read_to_string(&path)
                .expect("the script reads")
                .contains("benchdown session restore")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_guard_starts_and_answers_a_request() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = FakeRunner::quiet(clock);
        let script = script_path();
        let root = temporary("start");
        std::fs::create_dir_all(&root).expect("the folder creates");
        let (guard_log, state, status, out_status) = paths(&root);
        let mut guard = Guard::start(
            &runner,
            &script,
            &GuardPaths {
                guard_log: &guard_log,
                state: &state,
                status: &status,
                out_status: &out_status,
            },
            None,
        )
        .expect("the guard starts");

        let reply = guard
            .request("apply spotlight /")
            .expect("the request answers");
        assert!(reply.succeeded());
        assert_eq!(
            runner.guard().expect("a guard was started").requests(),
            ["apply spotlight /"]
        );
        let closed = guard.finish().expect("the guard ends");
        assert!(closed.succeeded());
        assert_eq!(
            runner.guard().expect("a guard was started").requests(),
            ["apply spotlight /", "END"]
        );
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_guard_that_does_not_say_ready_is_refused() {
        let clock = Arc::new(FakeClock::new(0));
        let mut runner = FakeRunner::quiet(clock);
        runner.set_guard_start_line("not ready");
        let script = script_path();
        let root = temporary("not-ready");
        std::fs::create_dir_all(&root).expect("the folder creates");
        let (guard_log, state, status, out_status) = paths(&root);

        let error = Guard::start(
            &runner,
            &script,
            &GuardPaths {
                guard_log: &guard_log,
                state: &state,
                status: &status,
                out_status: &out_status,
            },
            None,
        )
        .err()
        .expect("the guard is refused");
        assert!(error.contains("unexpected first line"), "{error}");
        assert!(error.contains("not ready"), "{error}");
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_guard_that_dies_mid_request_is_reported() {
        let clock = Arc::new(FakeClock::new(0));
        let runner = FakeRunner::quiet(clock);
        let script = script_path();
        let root = temporary("dies");
        std::fs::create_dir_all(&root).expect("the folder creates");
        let (guard_log, state, status, out_status) = paths(&root);
        let mut guard = Guard::start(
            &runner,
            &script,
            &GuardPaths {
                guard_log: &guard_log,
                state: &state,
                status: &status,
                out_status: &out_status,
            },
            None,
        )
        .expect("the guard starts");
        runner
            .guard()
            .expect("a guard was started")
            .die_on
            .lock()
            .expect("the guard is not poisoned")
            .insert("apply wifi en0".to_owned());

        let error = guard
            .request("apply wifi en0")
            .expect_err("the death shows");
        assert!(error.contains("guard died"), "{error}");
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_dir_all(&root);
    }

    // The script tests run the real `/bin/sh` guard with fake mdutil, tmutil and
    // networksetup commands on a temporary PATH. They touch no system setting.

    struct ScriptedGuard {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: BufReader<ChildStdout>,
        record: PathBuf,
        state: PathBuf,
        status: PathBuf,
        out_status: PathBuf,
        log: PathBuf,
        root: PathBuf,
        script: PathBuf,
    }

    impl ScriptedGuard {
        fn start(fail_undo: bool) -> Self {
            let root = temporary("script");
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).expect("the fake bin creates");
            let record = root.join("commands.log");
            let script = root.join("guard.sh");
            write_script(&script).expect("the guard script writes");
            std::fs::write(&record, "").expect("the record creates");

            write_fake(
                &bin,
                "mdutil",
                "if [ \"${FAIL_UNDO:-0}\" = \"1\" ] && [ \"$1\" = \"-i\" ] && [ \"$2\" = \"on\" ]; then\n  printf 'mdutil failed\\n' >&2\n  exit 1\nfi\nexit 0\n",
            );
            write_fake(&bin, "tmutil", "exit 0\n");
            write_fake(&bin, "networksetup", "exit 0\n");

            let state = root.join("state.json");
            let status = root.join("status.txt");
            let out_status = root.join("out-status.txt");
            let log = root.join("guard.log");
            std::fs::write(&state, "{}\n").expect("the state writes");
            std::fs::write(&status, "old status\n").expect("the status writes");
            std::fs::write(&out_status, "old status\n").expect("the status writes");
            std::fs::write(&log, "").expect("the log creates");

            let mut command = Command::new("/bin/sh");
            command
                .arg(&script)
                .arg(&log)
                .arg(&state)
                .arg(&status)
                .arg(&out_status)
                .env(
                    "BENCHDOWN_SESSION_GUARD_PATH",
                    format!("{}:/usr/bin:/usr/sbin:/bin:/sbin", bin.display()),
                )
                .env("GUARD_RECORD", &record)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if fail_undo {
                command.env("FAIL_UNDO", "1");
            }
            let mut child = command.spawn().expect("the guard starts");
            let stdin = child.stdin.take();
            let stdout = BufReader::new(child.stdout.take().expect("the guard has stdout"));
            Self {
                child,
                stdin,
                stdout,
                record,
                state,
                status,
                out_status,
                log,
                root,
                script,
            }
        }

        fn read_line(&mut self) -> String {
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("the guard answers");
            line.trim_end().to_owned()
        }

        fn write(&mut self, line: &str) {
            let stdin = self.stdin.as_mut().expect("the guard's input is open");
            writeln!(stdin, "{line}").expect("the request writes");
            stdin.flush().expect("the request flushes");
        }

        fn request(&mut self, line: &str) -> String {
            self.write(line);
            self.read_line()
        }

        fn close_input(&mut self) {
            self.stdin.take();
        }

        fn wait(&mut self) -> (i32, String) {
            self.stdin.take();
            let status = self.child.wait().expect("the guard exits");
            let mut stderr = String::new();
            self.child
                .stderr
                .take()
                .expect("the guard has stderr")
                .read_to_string(&mut stderr)
                .expect("stderr reads");
            (status.code().unwrap_or(-1), stderr)
        }

        fn records(&self) -> Vec<String> {
            std::fs::read_to_string(&self.record)
                .expect("the record reads")
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn status(&self) -> String {
            std::fs::read_to_string(&self.status).expect("the status reads")
        }
    }

    impl Drop for ScriptedGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_file(&self.script);
        }
    }

    fn write_fake(bin: &Path, name: &str, body: &str) {
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s %s\\n' \"{name}\" \"$*\" >> \"$GUARD_RECORD\"\n{body}"),
        )
        .expect("the fake command writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("the fake command is executable");
        }
    }

    fn signal(pid: u32, signal: &str) {
        let status = Command::new("kill")
            .args([signal, &pid.to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success(), "kill {signal} {pid} failed");
    }

    #[test]
    fn the_script_applies_and_undoes_through_the_protocol() {
        let mut guard = ScriptedGuard::start(false);
        assert_eq!(guard.read_line(), READY_LINE);
        for request in [
            "apply spotlight /",
            "apply spotlight /Volumes/My Disk",
            "apply time-machine",
            "apply wifi en0",
            "undo wifi en0",
            "undo time-machine",
            "undo spotlight /Volumes/My Disk",
            "undo spotlight /",
        ] {
            let reply = guard.request(request);
            assert_eq!(reply, "0", "{request}");
        }
        guard.write("END");
        guard.close_input();
        assert_eq!(guard.wait().0, 0);
        assert_eq!(
            guard.records(),
            [
                "mdutil -i off /",
                "mdutil -i off /Volumes/My Disk",
                "tmutil disable",
                "networksetup -setairportpower en0 off",
                "networksetup -setairportpower en0 on",
                "tmutil enable",
                "mdutil -i on /Volumes/My Disk",
                "mdutil -i on /",
            ]
        );
        assert!(guard.state.exists(), "END leaves the state file alone");
    }

    #[test]
    fn the_script_rejects_unknown_requests_and_missing_arguments() {
        let mut guard = ScriptedGuard::start(false);
        assert_eq!(guard.read_line(), READY_LINE);
        for request in [
            "apply nonsense /",
            "undo spotlight",
            "sh -c true",
            "apply spotlight -E /",
            "undo wifi -setairportpower",
        ] {
            let reply = guard.request(request);
            assert!(reply.starts_with("2 "), "{request}: {reply}");
        }
        guard.write("END");
        guard.close_input();
        assert_eq!(guard.wait().0, 0);
        assert!(guard.records().is_empty());
    }

    #[test]
    fn a_closed_pipe_without_end_restores_every_applied_change() {
        let mut guard = ScriptedGuard::start(false);
        assert_eq!(guard.read_line(), READY_LINE);
        for request in ["apply spotlight /", "apply time-machine", "apply wifi en0"] {
            assert_eq!(guard.request(request), "0", "{request}");
        }
        guard.close_input();
        assert_eq!(guard.wait().0, 0);
        assert_eq!(
            guard.records(),
            [
                "mdutil -i off /",
                "tmutil disable",
                "networksetup -setairportpower en0 off",
                "networksetup -setairportpower en0 on",
                "tmutil enable",
                "mdutil -i on /",
            ]
        );
        assert!(
            !guard.state.exists(),
            "a full restore removes the state file"
        );
        let status = guard.status();
        assert!(status.contains("The wrapper ended unexpectedly without sending END."));
        assert!(status.contains("Undo spotlight /: exit 0"));
        assert_eq!(
            std::fs::read_to_string(&guard.out_status).expect("the out status reads"),
            status
        );
        let log = std::fs::read_to_string(&guard.log).expect("the log reads");
        assert!(log.contains("command: mdutil -i off /"), "{log}");
        assert!(log.contains("exit 0"), "{log}");
        for line in log.lines() {
            assert!(
                line.len() > 21 && line.starts_with('[') && line.as_bytes()[20] == b']',
                "not a timestamped line: {line}"
            );
        }
    }

    #[test]
    fn a_failed_undo_keeps_the_state_file_and_reports_it() {
        let mut guard = ScriptedGuard::start(true);
        assert_eq!(guard.read_line(), READY_LINE);
        assert_eq!(guard.request("apply spotlight /"), "0");
        assert_eq!(guard.request("apply time-machine"), "0");
        guard.close_input();
        assert_eq!(guard.wait().0, 1);
        assert_eq!(
            guard.records(),
            [
                "mdutil -i off /",
                "tmutil disable",
                "tmutil enable",
                "mdutil -i on /",
            ]
        );
        assert!(guard.state.exists());
        let status = guard.status();
        assert!(
            status.contains("Undo spotlight /: exit 1 mdutil failed"),
            "{status}"
        );
        assert!(status.contains("Restore state file kept"), "{status}");
        assert!(status.contains("benchdown session restore"), "{status}");
        assert!(
            std::fs::read_to_string(&guard.log)
                .expect("the log reads")
                .contains("run: benchdown session restore")
        );
    }

    #[test]
    fn the_script_ignores_sighup() {
        let mut guard = ScriptedGuard::start(false);
        assert_eq!(guard.read_line(), READY_LINE);
        assert_eq!(guard.request("apply spotlight /"), "0");
        signal(guard.child.id(), "-HUP");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(guard.request("apply time-machine"), "0");
        guard.write("END");
        guard.close_input();
        assert_eq!(guard.wait().0, 0);
        assert!(guard.records().iter().any(|line| line == "tmutil disable"));
    }

    #[test]
    fn the_script_restores_on_sigterm() {
        let mut guard = ScriptedGuard::start(false);
        assert_eq!(guard.read_line(), READY_LINE);
        assert_eq!(guard.request("apply spotlight /"), "0");
        assert_eq!(guard.request("apply time-machine"), "0");
        signal(guard.child.id(), "-TERM");
        assert_eq!(guard.wait().0, 143);
        assert_eq!(
            guard.records(),
            [
                "mdutil -i off /",
                "tmutil disable",
                "tmutil enable",
                "mdutil -i on /",
            ]
        );
        assert!(!guard.state.exists());
        assert!(guard.status().contains("received SIGTERM"));
    }
}
