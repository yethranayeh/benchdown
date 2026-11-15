//! The machine facts a run needs, on the platform that is not macOS.
//!
//! The macOS run path reads them from `pmset`, `sysctl` and `ps`; Linux has the
//! same facts in `/proc`, `/sys` and a different `ps` spelling. The parsing
//! lives here and is compiled on every platform so its tests run here; only the
//! file reads are Linux-only.

/// Linux machine state.
#[cfg(any(target_os = "linux", test))]
pub mod linux {
    use std::path::Path;

    use crate::preflight::{
        BUSIEST_PROCESS_COUNT, BatteryState, PROCESS_LINE_WIDTH, ThermalState,
        UNKNOWN_BATTERY_STATE,
    };

    /// Where the kernel keeps the load averages.
    #[cfg(target_os = "linux")]
    pub const LOAD_PATH: &str = "/proc/loadavg";

    /// Where the kernel lists power supplies.
    #[cfg(target_os = "linux")]
    pub const POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";

    /// The product name the DMI tables expose.
    #[cfg(target_os = "linux")]
    pub const HARDWARE_MODEL_PATH: &str = "/sys/devices/virtual/dmi/id/product_name";

    /// The power source recorded when the machine reports mains power.
    pub const AC_POWER: &str = "AC Power";

    /// The power source recorded when the battery is discharging.
    pub const BATTERY_POWER: &str = "Battery Power";

    /// The three load averages from `/proc/loadavg`.
    pub fn parse_loadavg(text: &str) -> Result<Vec<f64>, String> {
        let values: Vec<f64> = text
            .split_whitespace()
            .take(3)
            .filter_map(|value| value.parse().ok())
            .collect();
        if values.len() < 3 {
            return Err(format!(
                "Could not parse load averages from: {}",
                text.trim()
            ));
        }
        Ok(values)
    }

    /// Reads the power supplies and the battery under `base`.
    ///
    /// A machine with no power-supply entries is a desktop on mains: it reports
    /// AC power and no battery rather than failing.
    pub fn power_supply(base: &Path) -> Result<(String, BatteryState), String> {
        let entries = match std::fs::read_dir(base) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((AC_POWER.to_owned(), BatteryState::absent()));
            }
            Err(error) => return Err(format!("{}: {error}", base.display())),
        };

        let mut on_mains = false;
        let mut battery: Option<BatteryState> = None;
        for entry in entries.flatten() {
            let directory = entry.path();
            let kind = read_trimmed(&directory.join("type")).unwrap_or_default();
            if kind == "Battery" {
                let percent = read_trimmed(&directory.join("capacity"))
                    .and_then(|value| value.parse::<u8>().ok());
                let state = read_trimmed(&directory.join("status"))
                    .map(|value| value.to_ascii_lowercase())
                    .unwrap_or_else(|| UNKNOWN_BATTERY_STATE.to_owned());
                battery = Some(BatteryState {
                    present: true,
                    percent,
                    state,
                });
            } else if read_trimmed(&directory.join("online")).as_deref() == Some("1") {
                on_mains = true;
            }
        }

        let source = if on_mains || battery.is_none() {
            AC_POWER
        } else {
            BATTERY_POWER
        };
        Ok((
            source.to_owned(),
            battery.unwrap_or_else(BatteryState::absent),
        ))
    }

    /// The busiest processes from `ps -eo pcpu,comm --sort=-pcpu`.
    pub fn parse_processes(text: &str) -> Vec<String> {
        text.lines()
            .skip(1)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(BUSIEST_PROCESS_COUNT)
            .map(|line| {
                let end = line
                    .char_indices()
                    .nth(PROCESS_LINE_WIDTH)
                    .map(|(index, _)| index)
                    .unwrap_or(line.len());
                line[..end].to_owned()
            })
            .collect()
    }

    /// The machine model from the DMI tables, or `unknown`.
    pub fn hardware_model(path: &Path) -> String {
        read_trimmed(path)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// The kernel release, from `uname -r`, which both platforms run.
    pub fn os_build(text: &str) -> String {
        let build = text.trim();
        if build.is_empty() {
            "unknown".to_owned()
        } else {
            build.to_owned()
        }
    }

    /// Linux thermal zones expose temperatures, which the record has no field
    /// for, so every limit stays null rather than being invented.
    pub fn thermal_state() -> ThermalState {
        ThermalState::none()
    }

    /// Holds an idle and sleep inhibitor while the run is active.
    ///
    /// Absent on machines without systemd, and a missing inhibitor must never
    /// stop a measurement, so a failure to start one is only reported.
    #[cfg(target_os = "linux")]
    pub fn keep_awake(system: &crate::runner::System) -> Option<u32> {
        let arguments = [
            "systemd-inhibit",
            "--what=idle:sleep",
            "--mode=block",
            "--why=benchdown is measuring",
            "tail",
            "-f",
            "/dev/null",
        ]
        .map(str::to_owned);
        match system.runner.spawn(&arguments) {
            Ok(spawned) => Some(spawned.pid),
            Err(error) => {
                println!("  note: no idle inhibitor ({error}); the machine may sleep");
                None
            }
        }
    }

    fn read_trimmed(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| text.trim().to_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        /// A throwaway directory, unique per test, under the system temp dir.
        fn scratch(name: &str) -> PathBuf {
            let path = std::env::temp_dir()
                .join(format!("benchdown-platform-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is created");
            path
        }

        fn supply(base: &Path, name: &str, fields: &[(&str, &str)]) {
            let directory = base.join(name);
            std::fs::create_dir_all(&directory).expect("the supply directory is created");
            for (field, value) in fields {
                std::fs::write(directory.join(field), value).expect("the field is written");
            }
        }

        #[test]
        fn the_three_load_averages_come_first() {
            let values =
                parse_loadavg("0.52 0.58 0.59 1/512 12345\n").expect("the kernel line parses");
            assert_eq!(values, [0.52, 0.58, 0.59]);
        }

        #[test]
        fn a_short_load_line_is_refused() {
            let error = parse_loadavg("0.52\n").expect_err("one value is not three");
            assert!(error.contains("Could not parse load averages"), "{error}");
        }

        #[test]
        fn a_laptop_on_mains_reports_ac_and_its_battery() {
            let base = scratch("ac");
            supply(&base, "AC", &[("type", "Mains"), ("online", "1")]);
            supply(
                &base,
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("capacity", "82"),
                    ("status", "Not charging"),
                ],
            );
            let (source, battery) = power_supply(&base).expect("the supplies read");
            assert_eq!(source, AC_POWER);
            assert_eq!(
                battery,
                BatteryState {
                    present: true,
                    percent: Some(82),
                    state: "not charging".to_owned(),
                }
            );
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_discharging_laptop_reports_its_battery() {
            let base = scratch("battery");
            supply(&base, "AC", &[("type", "Mains"), ("online", "0")]);
            supply(
                &base,
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("capacity", "41"),
                    ("status", "Discharging"),
                ],
            );
            let (source, battery) = power_supply(&base).expect("the supplies read");
            assert_eq!(source, BATTERY_POWER);
            assert_eq!(battery.percent, Some(41));
            assert_eq!(battery.state, "discharging");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_desktop_with_no_supplies_is_on_mains() {
            let base = scratch("desktop");
            let (source, battery) = power_supply(&base).expect("an empty directory is a desktop");
            assert_eq!(source, AC_POWER);
            assert_eq!(battery, BatteryState::absent());
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_missing_power_supply_directory_is_a_desktop_too() {
            let base = std::env::temp_dir().join("benchdown-platform-does-not-exist");
            let (source, battery) = power_supply(&base).expect("a missing directory is a desktop");
            assert_eq!(source, AC_POWER);
            assert_eq!(battery, BatteryState::absent());
        }

        #[test]
        fn a_battery_without_capacity_keeps_its_unknown_state() {
            let base = scratch("nocapacity");
            supply(&base, "BAT0", &[("type", "Battery"), ("status", "Full")]);
            let (_, battery) = power_supply(&base).expect("the supplies read");
            assert_eq!(battery.percent, None);
            assert_eq!(battery.state, "full");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn the_busiest_processes_stop_at_the_count() {
            let text = "%CPU COMMAND\n 45.0 chrome\n 30.0 rustc\n 10.0 ps\n 5.0 finder\n 2.0 mds\n 1.0 x\n 0.5 y\n";
            let lines = parse_processes(text);
            assert_eq!(
                lines,
                [
                    "45.0 chrome",
                    "30.0 rustc",
                    "10.0 ps",
                    "5.0 finder",
                    "2.0 mds"
                ]
            );
        }

        #[test]
        fn a_long_process_line_is_cut_to_the_width() {
            let name = "x".repeat(PROCESS_LINE_WIDTH * 2);
            let lines = parse_processes(&format!("%CPU COMMAND\n  1.0 {name}\n"));
            assert_eq!(lines[0].chars().count(), PROCESS_LINE_WIDTH);
        }

        #[test]
        fn the_hardware_model_reads_the_dmi_name() {
            let directory = scratch("dmi");
            let path = directory.join("product_name");
            std::fs::write(&path, "ThinkPad T14 Gen 3\n").expect("the name is written");
            assert_eq!(hardware_model(&path), "ThinkPad T14 Gen 3");
            assert_eq!(hardware_model(&directory.join("missing")), "unknown");
            let _ = std::fs::remove_dir_all(&directory);
        }

        #[test]
        fn an_empty_kernel_release_is_unknown() {
            assert_eq!(os_build("6.8.0-45-generic\n"), "6.8.0-45-generic");
            assert_eq!(os_build("  \n"), "unknown");
        }

        #[test]
        fn the_thermal_state_is_empty_rather_than_invented() {
            assert_eq!(thermal_state(), ThermalState::none());
        }
    }
}
