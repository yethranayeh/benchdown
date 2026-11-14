//! Session tests, kept out of the module to leave the orchestration readable.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::FakeClock;
use crate::runner::Captured;
use crate::session::fakes::{FakeRunner, FakeSignals, ok, text};
use crate::session::jobs::JobSpec;
use crate::session::sampler::{SESSION_SAMPLES_ENV, read_samples_file};
use crate::session::settings::SettingFlags;
use crate::session::state::{ChangeKind, RestoreEntry, RestoreState, write as write_state};

use super::*;

fn root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "benchdown-session-{}-{serial}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("the test root creates");
    path
}

fn job(name: &str, command: &str) -> JobSpec {
    JobSpec {
        name: name.to_owned(),
        command: command.to_owned(),
        estimate_minutes: Some(1.0),
        argv: None,
    }
}

struct Harness {
    options: SessionOptions,
    output: Arc<Mutex<String>>,
    signals: Arc<FakeSignals>,
    root: PathBuf,
}

impl Harness {
    fn output(&self) -> String {
        self.output
            .lock()
            .expect("the output is not poisoned")
            .clone()
    }

    fn run(&self) -> SessionOutcome {
        run_session(self.options.clone())
    }
}

fn harness(
    runner: Arc<FakeRunner>,
    clock: Arc<FakeClock>,
    root: PathBuf,
    settings: SettingFlags,
    jobs: Vec<JobSpec>,
) -> Harness {
    let output = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&output);
    let write: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |text: &str| {
        sink.lock()
            .expect("the output is not poisoned")
            .push_str(text);
    });
    let signals = Arc::new(FakeSignals::new());
    let options = SessionOptions {
        runner: runner.clone(),
        clock,
        settings,
        jobs,
        stop_on_failure: false,
        dry_run: false,
        out_dir: root.join("out"),
        cwd: root.clone(),
        state_path: root.join("state").join("restore.json"),
        session_id: "20261108-200000".to_owned(),
        started_at_ms: 1_794_132_000_000,
        sample_seconds: 5,
        sudo_refresh_ms: 60_000,
        notify: false,
        write,
        signals: signals.clone(),
    };
    Harness {
        options,
        output,
        signals,
        root,
    }
}

fn destination_plist() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n<key>Destinations</key><array><dict></dict></array></dict>\n</plist>\n"
}

fn settings_machine(clock: Arc<FakeClock>) -> FakeRunner {
    FakeRunner::new(clock, |argv| match argv.join(" ").as_str() {
        "mdutil -s -a" => {
            text("/:\n\tIndex is read-only.\n/System/Volumes/Data:\n\tIndexing enabled. \n")
        }
        "tmutil destinationinfo -X" => text(destination_plist()),
        "defaults read /Library/Preferences/com.apple.TimeMachine AutoBackup" => text("1\n"),
        "networksetup -listallhardwareports" => {
            text("Hardware Port: Ethernet\nDevice: en9\n\nHardware Port: Wi-Fi\nDevice: en0\n")
        }
        "networksetup -getairportpower en0" => text("Wi-Fi Power (en0): On\n"),
        "sysctl -n vm.loadavg" => text("{ 2.50 2.00 1.50 }\n"),
        "ps -A -r -o pcpu=,rss=,pid=,comm=" => {
            text(" 42.0 51200 101 /usr/libexec/mds_stores\n  1.5 2048 202 /usr/sbin/idle\n")
        }
        _ => ok(),
    })
}

fn all_settings() -> SettingFlags {
    SettingFlags {
        spotlight: true,
        time_machine: true,
        wifi: true,
    }
}

const DATA_VOLUME: &str = "/System/Volumes/Data";

#[test]
fn prior_states_are_read_first_and_only_enabled_settings_change() {
    let clock = Arc::new(FakeClock::new(1_794_132_000_000));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("all-on"),
        all_settings(),
        vec![job("build", "echo built")],
    );
    let state_path = h.options.state_path.clone();
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0, "{}", h.output());
    let guard = runner.guard().expect("a guard was started");
    assert_eq!(
        guard.requests(),
        [
            format!("apply spotlight {DATA_VOLUME}"),
            "apply time-machine".to_owned(),
            "apply wifi en0".to_owned(),
            "undo wifi en0".to_owned(),
            "undo time-machine".to_owned(),
            format!("undo spotlight {DATA_VOLUME}"),
            "END".to_owned(),
        ]
    );
    let calls = runner.calls();
    let guard_start = calls
        .iter()
        .position(|call| call.method == "pipe")
        .expect("the guard starts");
    for read in [
        "mdutil -s -a",
        "tmutil destinationinfo -X",
        "defaults read /Library/Preferences/com.apple.TimeMachine AutoBackup",
        "networksetup -listallhardwareports",
        "networksetup -getairportpower en0",
    ] {
        let at = calls
            .iter()
            .position(|call| call.argv.join(" ") == read)
            .unwrap_or_else(|| panic!("{read} was not read"));
        assert!(at < guard_start, "{read} was read after the guard started");
    }
    assert!(
        !calls.iter().any(|call| call.method == "capture"
            && call.argv.first().map(String::as_str) == Some("sudo")),
        "a change did not go through sudo directly"
    );
    assert_eq!(
        runner
            .interactives
            .lock()
            .expect("the records are not poisoned")
            .clone(),
        [vec!["sudo".to_owned(), "-v".to_owned()]]
    );
    let jobs = runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].process_group);
    assert_eq!(jobs[0].cwd.as_deref(), Some(h.root.as_path()));
    assert_eq!(
        jobs[0]
            .env
            .iter()
            .find(|(key, _)| key == SESSION_SAMPLES_ENV)
            .map(|(_, value)| value.clone()),
        Some(
            outcome
                .session_dir
                .join("samples.tsv")
                .display()
                .to_string()
        )
    );
    assert_eq!(outcome.restores.len(), 3);
    assert!(outcome.restored_all);
    assert!(!state_path.exists());
    assert!(
        h.output().contains("Started the restore guard"),
        "{}",
        h.output()
    );
}

#[test]
fn settings_that_are_already_off_are_not_changed() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("already-off"),
        all_settings(),
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0, "{}", h.output());
    assert!(runner.guard().is_none(), "no change means no guard");
    assert!(outcome.restores.is_empty());
    assert!(!state_path.exists());
    assert!(
        !runner
            .captures
            .lock()
            .expect("the records are not poisoned")
            .iter()
            .any(|argv| argv.join(" ").contains("mdutil -i off")),
        "no setting was changed"
    );
}

#[test]
fn a_failed_job_is_recorded_and_the_next_job_runs() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    runner.set_job_exit("sh -c bad", 1);
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("job-failure"),
        SettingFlags::default(),
        vec![job("bad", "bad"), job("good", "good")],
    );
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome
            .jobs
            .iter()
            .map(|result| result.job.name.as_str())
            .collect::<Vec<_>>(),
        ["bad", "good"]
    );
    assert_eq!(outcome.jobs[0].exit_code, 1);
    assert_eq!(outcome.jobs[1].exit_code, 0);
}

#[test]
fn stop_on_failure_skips_the_remaining_jobs() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    runner.set_job_exit("sh -c bad", 1);
    let h = harness(
        runner.clone(),
        clock,
        root("stop-on-failure"),
        SettingFlags::default(),
        vec![job("bad", "bad"), job("never", "never")],
    );
    let mut options = h.options.clone();
    options.stop_on_failure = true;
    let outcome = run_session(options);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome
            .jobs
            .iter()
            .map(|result| result.job.name.as_str())
            .collect::<Vec<_>>(),
        ["bad"]
    );
    assert_eq!(runner.jobs().len(), 1);
}

#[test]
fn a_state_file_blocks_a_new_session() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("blocked"),
        SettingFlags::default(),
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let state = RestoreState {
        version: 1,
        session_id: "20261107-120000".to_owned(),
        created_at: "2026-11-07T11:00:00Z".to_owned(),
        changes: vec![RestoreEntry {
            id: "spotlight:/".to_owned(),
            kind: ChangeKind::Spotlight,
            label: "Spotlight /".to_owned(),
            prior_state: "on".to_owned(),
            undo: ["sudo", "mdutil", "-i", "on", "/"]
                .map(str::to_owned)
                .to_vec(),
            undo_command: "sudo mdutil -i on /".to_owned(),
            applied: Some(true),
            applied_exit_code: Some(0),
        }],
    };
    write_state(&state_path, &state).expect("the state writes");
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.jobs.is_empty());
    assert!(
        runner.calls().is_empty(),
        "nothing runs while a restore is pending"
    );
    assert!(h.output().contains("Run: benchdown session restore"));
    assert!(state_path.exists());
}

#[test]
fn a_guard_that_cannot_start_stops_before_any_change() {
    let clock = Arc::new(FakeClock::new(0));
    let mut runner = settings_machine(Arc::clone(&clock));
    runner.set_guard_start_line("not ready");
    let runner = Arc::new(runner);
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("guard-fails"),
        all_settings(),
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.jobs.is_empty());
    assert!(!state_path.exists());
    assert!(
        !runner
            .captures
            .lock()
            .expect("the records are not poisoned")
            .iter()
            .any(|argv| argv.join(" ").contains("mdutil -i off")),
        "no setting was changed"
    );
    let output = h.output();
    assert!(output.contains("restore guard did not start"), "{output}");
    assert!(output.contains("not ready"), "{output}");
}

#[test]
fn a_guard_that_dies_during_the_session_falls_back_to_direct_sudo() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    runner.die_on_guard_request("apply wifi en0");
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("guard-dies"),
        all_settings(),
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0, "{}", h.output());
    let captures = runner
        .captures
        .lock()
        .expect("the records are not poisoned")
        .clone();
    assert!(
        captures
            .iter()
            .any(|argv| argv == &["sudo", "networksetup", "-setairportpower", "en0", "off"])
    );
    assert!(
        captures
            .iter()
            .any(|argv| argv == &["sudo", "networksetup", "-setairportpower", "en0", "on"])
    );
    assert_eq!(outcome.restores.len(), 3);
    assert!(outcome.restores.iter().all(|result| result.ok));
    assert!(outcome.restored_all);
    assert!(!state_path.exists());
    assert!(h.output().contains("directly with sudo"));
}

#[test]
fn a_failed_restore_keeps_the_state_file_and_does_not_stop_the_others() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    runner.fail_guard_request(&format!("undo spotlight {DATA_VOLUME}"), 1, "failed");
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("restore-fails"),
        SettingFlags {
            spotlight: true,
            time_machine: false,
            wifi: true,
        },
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let mut options = h.options.clone();
    options.notify = true;
    let outcome = run_session(options);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.restores.len(), 2);
    assert_eq!(
        outcome.restores.iter().filter(|result| result.ok).count(),
        1
    );
    assert!(!outcome.restored_all);
    assert!(state_path.exists(), "the failed undo keeps the state file");
    let output = h.output();
    assert!(
        output.contains("Not every system setting was restored"),
        "{output}"
    );
    assert!(output.contains("Manual undo commands"), "{output}");
    assert!(
        runner
            .captures
            .lock()
            .expect("the records are not poisoned")
            .iter()
            .any(|argv| argv.first().map(String::as_str) == Some("osascript")),
        "the owner is notified"
    );
}

#[test]
fn a_signal_stops_the_running_job_restores_once_and_exits_with_its_code() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    runner.hang("sh -c sleep 60");
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("signal"),
        SettingFlags {
            spotlight: true,
            time_machine: false,
            wifi: false,
        },
        vec![job("slow", "sleep 60")],
    );
    let state_path = h.options.state_path.clone();
    let signals = Arc::clone(&h.signals);
    let running = std::thread::spawn(move || h.run());

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while runner.jobs_started().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the job did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    signals.emit(SessionSignal::Interrupt);
    let outcome = running.join().expect("the session finishes");

    assert_eq!(outcome.exit_code, 130);
    assert!(runner.jobs_started()[0].was_stopped());
    let guard = runner.guard().expect("a guard was started");
    assert!(
        guard
            .requests()
            .contains(&format!("undo spotlight {DATA_VOLUME}")),
        "{:?}",
        guard.requests()
    );
    assert_eq!(guard.requests().last().map(String::as_str), Some("END"));
    assert_eq!(outcome.restores.len(), 1);
    assert!(outcome.restored_all);
    assert!(!state_path.exists());
}

#[test]
fn dry_run_reads_state_but_changes_nothing_and_writes_no_files() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("dry-run"),
        all_settings(),
        vec![job("plan-only", "echo planned")],
    );
    let mut options = h.options.clone();
    options.dry_run = true;
    options.jobs[0].estimate_minutes = Some(3.0);
    let out_dir = options.out_dir.clone();
    let state_path = options.state_path.clone();
    let outcome = run_session(options);

    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.jobs.is_empty());
    assert!(outcome.restores.is_empty());
    assert!(
        !out_dir.join("sessions").exists(),
        "no session folder is written"
    );
    assert!(!state_path.exists());
    let calls = runner.calls();
    assert!(!calls.is_empty());
    assert!(
        calls.iter().all(|call| call.method == "capture"),
        "a dry run only reads: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.argv.first().map(String::as_str) == Some("sudo")),
        "a dry run asks for no password"
    );
    let output = h.output();
    assert!(output.contains("plan-only"), "{output}");
    assert!(output.contains("echo planned"), "{output}");
    assert!(output.contains("estimate 3 min"), "{output}");
    assert!(output.contains("Dry run"), "{output}");
    assert!(output.contains("Restore guard"), "{output}");
}

#[test]
fn a_failed_change_still_restores_and_removes_the_state() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    runner.fail_guard_request(
        &format!("apply spotlight {DATA_VOLUME}"),
        1,
        "mdutil failed",
    );
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("change-fails"),
        SettingFlags {
            spotlight: true,
            time_machine: false,
            wifi: false,
        },
        vec![job("job", "echo ok")],
    );
    let state_path = h.options.state_path.clone();
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.restores.len(), 1);
    assert!(outcome.restores[0].ok);
    assert!(outcome.restored_all);
    assert!(!state_path.exists());
    assert!(h.output().contains("mdutil failed"));
}

#[test]
fn the_log_and_status_files_carry_job_output_and_the_final_paths() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    runner.set_job_output("sh -c talk", &["hello from the job"]);
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("log"),
        SettingFlags::default(),
        vec![job("talker", "talk")],
    );
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0);
    let log = std::fs::read_to_string(&outcome.log_path).expect("the log reads");
    assert!(log.contains("[talker] hello from the job"), "{log}");
    for line in log.lines() {
        assert!(
            line.len() > 10 && line.starts_with('[') && line.as_bytes()[9] == b']',
            "not a timestamped line: {line}"
        );
    }
    let status = std::fs::read_to_string(&outcome.status_path).expect("the status reads");
    assert!(status.contains("talker"), "{status}");
    assert!(
        status.contains(&outcome.log_path.display().to_string()),
        "{status}"
    );
    assert_eq!(
        std::fs::read_to_string(h.root.join("out").join("status.txt"))
            .expect("the out status reads"),
        status
    );
}

#[test]
fn a_session_samples_load_and_the_busiest_processes_into_its_folder() {
    let clock = Arc::new(FakeClock::new(1_794_132_005_000));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("samples"),
        SettingFlags::default(),
        vec![job("energy", "echo measured")],
    );
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0, "{}", h.output());
    let samples_path = outcome.session_dir.join("samples.tsv");
    let samples = read_samples_file(&samples_path).expect("the samples read");
    assert!(!samples.is_empty());
    assert_eq!(samples[0].load1, 2.5);
    assert_eq!(samples[0].processes[0].command, "/usr/libexec/mds_stores");
    let jobs = runner.jobs();
    assert_eq!(
        jobs[0]
            .env
            .iter()
            .find(|(key, _)| key == SESSION_SAMPLES_ENV)
            .map(|(_, value)| value.clone()),
        Some(samples_path.display().to_string())
    );
    assert!(
        h.output().contains(&samples_path.display().to_string()),
        "{}",
        h.output()
    );
}

#[test]
fn a_failing_sample_does_not_fail_the_session() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::new(Arc::clone(&clock), |argv| {
        if argv.first().map(String::as_str) == Some("ps") {
            Captured {
                exit_code: 1,
                stdout: String::new(),
                stderr: "ps failed".to_owned(),
            }
        } else {
            ok()
        }
    }));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("sample-failure"),
        SettingFlags::default(),
        vec![job("job", "echo ok")],
    );
    let outcome = h.run();

    assert_eq!(outcome.exit_code, 0);
    let samples = std::fs::read_to_string(outcome.session_dir.join("samples.tsv"))
        .expect("the samples file reads");
    assert_eq!(samples.lines().count(), 1, "only the header");
    assert!(h.output().contains("A load sample failed"));
}

#[test]
fn the_notification_says_what_happened() {
    let clock = Arc::new(FakeClock::new(0));
    let runner = Arc::new(FakeRunner::quiet(Arc::clone(&clock)));
    runner.set_job_exit("sh -c bad", 1);
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("notify"),
        SettingFlags::default(),
        vec![job("bad", "bad")],
    );
    let mut options = h.options.clone();
    options.notify = true;
    let outcome = run_session(options);
    assert_eq!(outcome.exit_code, 1);

    let captures = runner
        .captures
        .lock()
        .expect("the records are not poisoned")
        .clone();
    let script = captures
        .iter()
        .find(|argv| argv.first().map(String::as_str) == Some("osascript"))
        .expect("the notification runs");
    let script = script.join(" ");
    assert!(script.contains("did not succeed"), "{script}");
    assert!(script.contains("Everything restored."), "{script}");
}

#[test]
fn session_folder_ids_get_a_numeric_suffix_when_the_time_is_taken() {
    let out_dir = root("folder-id");
    let started_at_ms = 1_794_132_000_000;
    let first = session_folder_id(&out_dir, started_at_ms);
    std::fs::create_dir_all(out_dir.join("sessions").join(&first)).expect("the folder creates");
    assert_eq!(
        session_folder_id(&out_dir, started_at_ms),
        format!("{first}-1")
    );
}

#[test]
fn the_session_plan_shows_both_a_change_and_a_job_in_order() {
    let clock = Arc::new(FakeClock::new(1_794_132_000_000));
    let runner = Arc::new(settings_machine(Arc::clone(&clock)));
    let h = harness(
        Arc::clone(&runner),
        clock,
        root("plan-order"),
        SettingFlags {
            spotlight: true,
            time_machine: false,
            wifi: false,
        },
        vec![job("build", "echo built")],
    );
    let mut options = h.options.clone();
    options.dry_run = true;
    let outcome = run_session(options);
    assert_eq!(outcome.exit_code, 0);

    let output = h.output();
    let change = output
        .find("Spotlight indexing off")
        .expect("the change is planned");
    let job = output
        .find("Job build: echo built")
        .expect("the job is planned");
    assert!(change < job, "the change comes before the job");
}
