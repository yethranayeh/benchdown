//! Reads the prior state of each flagged setting and plans only the changes needed.

use crate::runner::{CaptureOptions, Captured, Runner};
use crate::session::format::command_line;
use crate::session::state::ChangeKind;

const TIME_MACHINE_PREFERENCES: &str = "/Library/Preferences/com.apple.TimeMachine";
const AUTO_BACKUP_KEY: &str = "AutoBackup";

/// The settings the session was asked to change. Nothing else is touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingFlags {
    pub spotlight: bool,
    pub time_machine: bool,
    pub wifi: bool,
}

/// One change the session will make, with the command that reverses it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedChange {
    pub id: String,
    pub kind: ChangeKind,
    pub label: String,
    pub prior_state: String,
    pub apply: Vec<String>,
    pub apply_command: String,
    pub undo: Vec<String>,
    pub undo_command: String,
    /// The words after `apply` or `undo` in a restore-guard request.
    pub guard: String,
    pub applied: Option<bool>,
    pub applied_exit_code: Option<i32>,
}

impl From<&PlannedChange> for crate::session::state::RestoreEntry {
    fn from(change: &PlannedChange) -> Self {
        Self {
            id: change.id.clone(),
            kind: change.kind,
            label: change.label.clone(),
            prior_state: change.prior_state.clone(),
            undo: change.undo.clone(),
            undo_command: change.undo_command.clone(),
            applied: change.applied,
            applied_exit_code: change.applied_exit_code,
        }
    }
}

/// What the reads decided: changes to make, notes for the log, reads for the plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingPlan {
    pub changes: Vec<PlannedChange>,
    pub notes: Vec<String>,
    pub reads: Vec<String>,
}

/// Reads each flagged setting and plans only the changes that are needed.
pub fn plan_setting_changes(
    runner: &dyn Runner,
    flags: SettingFlags,
    allow_sudo_reads: bool,
) -> SettingPlan {
    let mut plan = SettingPlan::default();
    if flags.spotlight {
        plan_spotlight(runner, &mut plan);
    }
    if flags.time_machine {
        plan_time_machine(runner, &mut plan, allow_sudo_reads);
    }
    if flags.wifi {
        plan_wifi(runner, &mut plan);
    }
    plan
}

fn capture(runner: &dyn Runner, argv: &[&str]) -> Result<Captured, String> {
    runner.capture(
        &argv
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>(),
        CaptureOptions::default(),
    )
}

/// What `mdutil -s -a` reports for one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeIndexing {
    On,
    Off,
    /// The sealed system volume reports this and cannot be switched.
    ReadOnly,
    /// The volume line had no indexing line this build understands.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeStatus {
    pub volume: String,
    pub indexing: VolumeIndexing,
}

/// Reads `mdutil -s -a` into one entry per volume.
pub fn parse_spotlight_status(output: &str) -> Vec<VolumeStatus> {
    let mut volumes = Vec::new();
    let mut current: Option<String> = None;
    let mut finish = |volume: Option<String>, indexing: VolumeIndexing| {
        if let Some(volume) = volume {
            volumes.push(VolumeStatus { volume, indexing });
        }
    };

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.ends_with(':') && !line.to_lowercase().contains("indexing") {
            finish(current.take(), VolumeIndexing::Unknown);
            current = Some(line.trim_end_matches(':').to_owned());
            continue;
        }
        if current.is_none() {
            continue;
        }
        let lower = line.to_lowercase();
        if lower.contains("index is read-only") {
            finish(current.take(), VolumeIndexing::ReadOnly);
        } else if lower.contains("indexing") && lower.contains("disabled") {
            finish(current.take(), VolumeIndexing::Off);
        } else if lower.contains("indexing") && lower.contains("enabled") {
            finish(current.take(), VolumeIndexing::On);
        }
    }
    finish(current, VolumeIndexing::Unknown);
    volumes
}

fn plan_spotlight(runner: &dyn Runner, plan: &mut SettingPlan) {
    let argv = ["mdutil", "-s", "-a"];
    plan.reads.push(command_line(&argv.map(str::to_owned)));
    let result = match capture(runner, &argv) {
        Ok(result) => result,
        Err(error) => {
            plan.notes.push(format!(
                "Spotlight: could not read indexing state, leaving it unchanged: {error}"
            ));
            return;
        }
    };
    if !result.succeeded() {
        plan.notes.push(format!(
            "Spotlight: could not read indexing state, leaving it unchanged{}",
            failure_detail(&result)
        ));
        return;
    }

    let volumes = parse_spotlight_status(&result.stdout);
    if volumes.is_empty() {
        plan.notes.push(
            "Spotlight: no volume indexing state was reported, leaving it unchanged".to_owned(),
        );
        return;
    }
    for volume in volumes {
        match volume.indexing {
            VolumeIndexing::ReadOnly => plan.notes.push(format!(
                "Spotlight: {} has a read-only index, leaving it unchanged",
                volume.volume
            )),
            VolumeIndexing::Unknown => plan.notes.push(format!(
                "Spotlight: could not read indexing state for {}, leaving it unchanged",
                volume.volume
            )),
            VolumeIndexing::Off => plan.notes.push(format!(
                "Spotlight: {} was already off, leaving it off",
                volume.volume
            )),
            VolumeIndexing::On => plan.changes.push(make_change(
                format!("spotlight:{}", volume.volume),
                ChangeKind::Spotlight,
                format!("Spotlight indexing off for {}", volume.volume),
                "on".to_owned(),
                vec![
                    "sudo".to_owned(),
                    "mdutil".to_owned(),
                    "-i".to_owned(),
                    "off".to_owned(),
                    volume.volume.clone(),
                ],
                vec![
                    "sudo".to_owned(),
                    "mdutil".to_owned(),
                    "-i".to_owned(),
                    "on".to_owned(),
                    volume.volume.clone(),
                ],
                format!("spotlight {}", volume.volume),
            )),
        }
    }
}

/// `tmutil destinationinfo -X` prints a plist with an empty top-level dict when
/// no destination is configured.
fn is_empty_plist_dict(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("<plist")
        && lower.contains("</plist>")
        && lower.contains("<dict>")
        && lower.contains("</dict>")
        && !lower.contains("<key>")
}

fn plan_time_machine(runner: &dyn Runner, plan: &mut SettingPlan, allow_sudo_reads: bool) {
    let destination_argv = ["tmutil", "destinationinfo", "-X"];
    plan.reads
        .push(command_line(&destination_argv.map(str::to_owned)));
    let destinations = match capture(runner, &destination_argv) {
        Ok(result) => result,
        Err(error) => {
            plan.notes.push(format!(
                "Time Machine: could not read its destinations, leaving it unchanged: {error}"
            ));
            return;
        }
    };
    if destinations.succeeded() && is_empty_plist_dict(&destinations.stdout) {
        plan.notes
            .push("Time Machine isn't set up, leaving it unchanged".to_owned());
        return;
    }

    let read_argv = [
        "defaults",
        "read",
        TIME_MACHINE_PREFERENCES,
        AUTO_BACKUP_KEY,
    ];
    plan.reads.push(command_line(&read_argv.map(str::to_owned)));
    let mut automatic = capture(runner, &read_argv)
        .ok()
        .and_then(|result| parse_boolean(&result));
    if automatic.is_none() && allow_sudo_reads {
        let elevated = ["sudo", "-n"]
            .into_iter()
            .chain(read_argv)
            .collect::<Vec<_>>();
        plan.reads.push(command_line(
            &elevated
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>(),
        ));
        automatic = capture(runner, &elevated)
            .ok()
            .and_then(|result| parse_boolean(&result));
    }

    match automatic {
        None => plan.notes.push(
            "Time Machine: could not read whether automatic backups were on, leaving it unchanged"
                .to_owned(),
        ),
        Some(false) => plan
            .notes
            .push("Time Machine: automatic backups were already off, leaving them off".to_owned()),
        Some(true) => plan.changes.push(make_change(
            "time-machine:automatic".to_owned(),
            ChangeKind::TimeMachine,
            "Time Machine automatic backups off".to_owned(),
            "on".to_owned(),
            vec!["sudo".to_owned(), "tmutil".to_owned(), "disable".to_owned()],
            vec!["sudo".to_owned(), "tmutil".to_owned(), "enable".to_owned()],
            "time-machine".to_owned(),
        )),
    }
}

fn parse_boolean(result: &Captured) -> Option<bool> {
    if !result.succeeded() {
        return None;
    }
    match result.stdout.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Finds the Wi-Fi device in `networksetup -listallhardwareports` output.
pub fn parse_wifi_device(output: &str) -> Option<String> {
    let mut block: Vec<&str> = Vec::new();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
            continue;
        }
        block.push(line);
    }
    if !block.is_empty() {
        blocks.push(block);
    }

    for block in blocks {
        let port = block.iter().find_map(|line| {
            let value = line.strip_prefix("Hardware Port:")?;
            Some(value.trim().to_lowercase())
        });
        if port.as_deref() != Some("wi-fi") && port.as_deref() != Some("airport") {
            continue;
        }
        if let Some(device) = block.iter().find_map(|line| line.strip_prefix("Device:")) {
            return Some(device.trim().to_owned());
        }
    }
    None
}

fn plan_wifi(runner: &dyn Runner, plan: &mut SettingPlan) {
    let hardware_argv = ["networksetup", "-listallhardwareports"];
    plan.reads
        .push(command_line(&hardware_argv.map(str::to_owned)));
    let hardware = match capture(runner, &hardware_argv) {
        Ok(result) => result,
        Err(error) => {
            plan.notes.push(format!(
                "Wi-Fi: could not list hardware ports, leaving it unchanged: {error}"
            ));
            return;
        }
    };
    if !hardware.succeeded() {
        plan.notes.push(format!(
            "Wi-Fi: could not list hardware ports, leaving it unchanged{}",
            failure_detail(&hardware)
        ));
        return;
    }
    let Some(device) = parse_wifi_device(&hardware.stdout) else {
        plan.notes
            .push("Wi-Fi: no Wi-Fi device was found, leaving it unchanged".to_owned());
        return;
    };

    let power_argv = ["networksetup", "-getairportpower", &device];
    plan.reads
        .push(command_line(&power_argv.map(str::to_owned)));
    let power = match capture(runner, &power_argv) {
        Ok(result) => result,
        Err(error) => {
            plan.notes.push(format!(
                "Wi-Fi: could not read power state for {device}, leaving it unchanged: {error}"
            ));
            return;
        }
    };
    if !power.succeeded() {
        plan.notes.push(format!(
            "Wi-Fi: could not read power state for {device}, leaving it unchanged{}",
            failure_detail(&power)
        ));
        return;
    }
    let lower = power.stdout.trim().to_lowercase();
    if lower.ends_with(": off") {
        plan.notes
            .push(format!("Wi-Fi: {device} was already off, leaving it off"));
        return;
    }
    if !lower.ends_with(": on") {
        plan.notes.push(format!(
            "Wi-Fi: power state for {device} was not reported, leaving it unchanged"
        ));
        return;
    }

    plan.changes.push(make_change(
        format!("wifi:{device}"),
        ChangeKind::Wifi,
        format!("Wi-Fi off for {device}"),
        "on".to_owned(),
        vec![
            "sudo".to_owned(),
            "networksetup".to_owned(),
            "-setairportpower".to_owned(),
            device.clone(),
            "off".to_owned(),
        ],
        vec![
            "sudo".to_owned(),
            "networksetup".to_owned(),
            "-setairportpower".to_owned(),
            device.clone(),
            "on".to_owned(),
        ],
        format!("wifi {device}"),
    ));
}

#[allow(clippy::too_many_arguments)]
fn make_change(
    id: String,
    kind: ChangeKind,
    label: String,
    prior_state: String,
    apply: Vec<String>,
    undo: Vec<String>,
    guard: String,
) -> PlannedChange {
    PlannedChange {
        id,
        kind,
        label,
        prior_state,
        apply_command: command_line(&apply),
        apply,
        undo_command: command_line(&undo),
        undo,
        guard,
        applied: None,
        applied_exit_code: None,
    }
}

fn failure_detail(result: &Captured) -> String {
    let stderr = result.stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    fn hardware_ports(device: Option<&str>) -> String {
        let mut text =
            "Hardware Port: Ethernet\nDevice: en9\nEthernet Address: aa:bb:cc:dd:ee:ff\n"
                .to_owned();
        if let Some(device) = device {
            text.push_str(&format!(
                "\nHardware Port: Wi-Fi\nDevice: {device}\nEthernet Address: 11:22:33:44:55:66\n"
            ));
        }
        text
    }

    fn empty_plist() -> String {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n</dict>\n</plist>\n".to_owned()
    }

    fn destination_plist() -> String {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n<key>Destinations</key><array><dict></dict></array></dict>\n</plist>\n".to_owned()
    }

    fn runner(
        spotlight: &str,
        automatic: Captured,
        destinations: &str,
        wifi_device: Option<&str>,
        wifi_power: &str,
    ) -> FakeRunner {
        let spotlight = spotlight.to_owned();
        let destinations = destinations.to_owned();
        let wifi_device = wifi_device.map(str::to_owned);
        let wifi_power = wifi_power.to_owned();
        FakeRunner::new(move |argv| match argv.join(" ").as_str() {
            "mdutil -s -a" => Captured {
                exit_code: 0,
                stdout: spotlight.clone(),
                stderr: String::new(),
            },
            "tmutil destinationinfo -X" => Captured {
                exit_code: 0,
                stdout: destinations.clone(),
                stderr: String::new(),
            },
            "networksetup -listallhardwareports" => Captured {
                exit_code: 0,
                stdout: hardware_ports(wifi_device.as_deref()),
                stderr: String::new(),
            },
            value if value.starts_with("networksetup -getairportpower") => Captured {
                exit_code: 0,
                stdout: wifi_power.clone(),
                stderr: String::new(),
            },
            "defaults read /Library/Preferences/com.apple.TimeMachine AutoBackup" => {
                automatic.clone()
            }
            "sudo -n defaults read /Library/Preferences/com.apple.TimeMachine AutoBackup" => {
                automatic.clone()
            }
            _ => Captured {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        })
    }

    fn all_on() -> FakeRunner {
        runner(
            "/:\n\tIndex is read-only.\n/System/Volumes/Data:\n\tIndexing enabled. \n",
            Captured {
                exit_code: 0,
                stdout: "1\n".to_owned(),
                stderr: String::new(),
            },
            &destination_plist(),
            Some("en0"),
            "Wi-Fi Power (en0): On\n",
        )
    }

    #[test]
    fn spotlight_status_keeps_on_off_and_read_only_volumes_apart() {
        let output = [
            "/:",
            "\tIndex is read-only.",
            "/System/Volumes/Data:",
            "\tIndexing enabled. ",
            "/Volumes/Backup:",
            "\tIndexing disabled.",
            "/Volumes/Odd:",
            "\tSomething new.",
            "",
        ]
        .join("\n");
        assert_eq!(
            parse_spotlight_status(&output),
            [
                VolumeStatus {
                    volume: "/".to_owned(),
                    indexing: VolumeIndexing::ReadOnly,
                },
                VolumeStatus {
                    volume: "/System/Volumes/Data".to_owned(),
                    indexing: VolumeIndexing::On,
                },
                VolumeStatus {
                    volume: "/Volumes/Backup".to_owned(),
                    indexing: VolumeIndexing::Off,
                },
                VolumeStatus {
                    volume: "/Volumes/Odd".to_owned(),
                    indexing: VolumeIndexing::Unknown,
                },
            ]
        );
    }

    #[test]
    fn every_enabled_setting_plans_one_change_with_its_undo() {
        let runner = all_on();
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: true,
                time_machine: true,
                wifi: true,
            },
            true,
        );

        assert_eq!(plan.changes.len(), 3, "{plan:?}");
        assert_eq!(plan.changes[0].id, "spotlight:/System/Volumes/Data");
        assert_eq!(plan.changes[0].guard, "spotlight /System/Volumes/Data");
        assert_eq!(
            plan.changes[0].apply,
            ["sudo", "mdutil", "-i", "off", "/System/Volumes/Data"]
        );
        assert_eq!(
            plan.changes[0].undo_command,
            "sudo mdutil -i on /System/Volumes/Data"
        );
        assert_eq!(plan.changes[1].guard, "time-machine");
        assert_eq!(plan.changes[1].apply_command, "sudo tmutil disable");
        assert_eq!(plan.changes[2].guard, "wifi en0");
        assert_eq!(
            plan.changes[2].undo_command,
            "sudo networksetup -setairportpower en0 on"
        );
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("/ has a read-only index")),
            "{:?}",
            plan.notes
        );
        for read in [
            "mdutil -s -a",
            "tmutil destinationinfo -X",
            "defaults read /Library/Preferences/com.apple.TimeMachine AutoBackup",
            "networksetup -listallhardwareports",
            "networksetup -getairportpower en0",
        ] {
            assert!(plan.reads.iter().any(|entry| entry == read), "{read}");
        }
    }

    #[test]
    fn settings_that_are_already_off_are_not_changed() {
        let runner = runner(
            "/System/Volumes/Data:\n\tIndexing disabled. \n",
            Captured {
                exit_code: 0,
                stdout: "0\n".to_owned(),
                stderr: String::new(),
            },
            &destination_plist(),
            Some("en0"),
            "Wi-Fi Power (en0): Off\n",
        );
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: true,
                time_machine: true,
                wifi: true,
            },
            true,
        );
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
        assert!(plan.notes.iter().any(|note| note.contains("already off")));
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("automatic backups were already off"))
        );
    }

    #[test]
    fn time_machine_without_a_destination_is_left_alone_without_reading_preferences() {
        let runner = runner(
            "",
            Captured {
                exit_code: 0,
                stdout: "1\n".to_owned(),
                stderr: String::new(),
            },
            &empty_plist(),
            None,
            "",
        );
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: false,
                time_machine: true,
                wifi: false,
            },
            true,
        );
        assert!(plan.changes.is_empty());
        assert!(
            plan.reads
                .iter()
                .any(|read| read == "tmutil destinationinfo -X")
        );
        assert!(
            !plan.reads.iter().any(|read| read.contains("defaults read")),
            "{:?}",
            plan.reads
        );
        assert!(
            plan.notes.iter().any(|note| note.contains("isn't set up")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn an_unreadable_time_machine_state_is_left_unchanged() {
        let runner = runner(
            "",
            Captured {
                exit_code: 1,
                stdout: String::new(),
                stderr: "not readable".to_owned(),
            },
            &destination_plist(),
            None,
            "",
        );
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: false,
                time_machine: true,
                wifi: false,
            },
            false,
        );
        assert!(plan.changes.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("could not read whether automatic backups")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_failed_spotlight_read_is_left_unchanged() {
        let runner = FakeRunner::new(|argv| Captured {
            exit_code: if argv.first().map(String::as_str) == Some("mdutil") {
                1
            } else {
                0
            },
            stdout: String::new(),
            stderr: "mdutil failed".to_owned(),
        });
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: true,
                time_machine: false,
                wifi: false,
            },
            false,
        );
        assert!(plan.changes.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("could not read indexing state")
                    && note.contains("mdutil failed")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_machine_without_wifi_reports_no_device() {
        let runner = runner(
            "",
            Captured {
                exit_code: 0,
                stdout: "0\n".to_owned(),
                stderr: String::new(),
            },
            &destination_plist(),
            None,
            "",
        );
        let plan = plan_setting_changes(
            &runner,
            SettingFlags {
                spotlight: false,
                time_machine: false,
                wifi: true,
            },
            true,
        );
        assert!(plan.changes.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("no Wi-Fi device")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn the_wifi_device_is_found_by_its_port_name() {
        assert_eq!(
            parse_wifi_device(&hardware_ports(Some("en1"))).as_deref(),
            Some("en1")
        );
        assert_eq!(parse_wifi_device(&hardware_ports(None)), None);
        let airport = "Hardware Port: Airport\nDevice: en2\n";
        assert_eq!(parse_wifi_device(airport).as_deref(), Some("en2"));
    }

    #[test]
    fn boolean_preferences_are_read_in_the_forms_defaults_writes() {
        for (text, expected) in [
            ("1\n", Some(true)),
            ("true", Some(true)),
            ("YES", Some(true)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("no", Some(false)),
            ("maybe", None),
        ] {
            let captured = Captured {
                exit_code: 0,
                stdout: text.to_owned(),
                stderr: String::new(),
            };
            assert_eq!(parse_boolean(&captured), expected, "{text}");
        }
        assert_eq!(
            parse_boolean(&Captured {
                exit_code: 1,
                stdout: "1".to_owned(),
                stderr: String::new()
            }),
            None
        );
    }
}
