//! Choosing where the power samples come from.

use std::fmt;

/// A power sensor benchdown can measure with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensor {
    /// `powermetrics` on macOS: processor, GPU and ANE power, needs sudo.
    Powermetrics,
    /// AppleSMC system power on macOS, no sudo.
    Smc,
    /// Intel or AMD RAPL energy counters on Linux, needs read access to the MSRs.
    Rapl,
}

/// The sensors this build can use, in the order help lists them.
pub const SENSORS: [Sensor; 3] = [Sensor::Powermetrics, Sensor::Smc, Sensor::Rapl];

impl Sensor {
    /// Reads a sensor name, without checking whether this platform has it.
    pub fn parse(value: &str) -> Result<Self, String> {
        SENSORS
            .iter()
            .copied()
            .find(|sensor| sensor.name() == value)
            .ok_or_else(|| {
                format!(
                    "unknown sensor {value}: use {}",
                    SENSORS
                        .iter()
                        .map(|sensor| sensor.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /// The sensor a run uses when `--sensor` is not given.
    pub fn default_for_platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::Powermetrics
        } else {
            Self::Rapl
        }
    }

    /// Resolves `--sensor`, defaulting per platform and refusing what this build lacks.
    pub fn resolve(value: Option<&str>) -> Result<Self, String> {
        let sensor = match value {
            Some(value) => Self::parse(value)?,
            None => Self::default_for_platform(),
        };
        if !sensor.available_here() {
            return Err(format!(
                "sensor {} is not available on {}: use {}",
                sensor.name(),
                std::env::consts::OS,
                Sensor::supported_here().join(", ")
            ));
        }
        Ok(sensor)
    }

    /// Whether this build can start this sensor.
    pub fn available_here(self) -> bool {
        match self {
            Self::Powermetrics | Self::Smc => cfg!(target_os = "macos"),
            Self::Rapl => cfg!(target_os = "linux"),
        }
    }

    /// The sensors this build can start.
    pub fn supported_here() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = SENSORS
            .iter()
            .filter(|sensor| sensor.available_here())
            .map(|sensor| sensor.name())
            .collect();
        if names.is_empty() {
            names.push("none");
        }
        names
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Powermetrics => "powermetrics",
            Self::Smc => "smc",
            Self::Rapl => "rapl",
        }
    }

    /// Only powermetrics needs a sudo ticket.
    pub fn needs_sudo(self) -> bool {
        matches!(self, Self::Powermetrics)
    }

    /// What the sensor measures, for the line every result carries.
    pub fn describes(self) -> &'static str {
        match self {
            Self::Powermetrics => "whole-machine processor power (CPU, GPU and ANE)",
            Self::Smc => "whole-machine system power (the SMC system rail, not processor-only)",
            Self::Rapl => "whole-machine package power (RAPL energy counters)",
        }
    }
}

impl fmt::Display for Sensor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_round_trips() {
        for sensor in SENSORS {
            assert_eq!(Sensor::parse(sensor.name()), Ok(sensor));
        }
    }

    #[test]
    fn an_unknown_name_lists_the_choices() {
        let error = Sensor::parse("gpu").expect_err("gpu is not a sensor");
        assert!(error.contains("unknown sensor gpu"), "{error}");
        assert!(error.contains("powermetrics, smc, rapl"), "{error}");
    }

    #[test]
    fn the_platform_default_is_the_one_that_needs_no_extra_setup() {
        let expected = if cfg!(target_os = "macos") {
            Sensor::Powermetrics
        } else {
            Sensor::Rapl
        };
        assert_eq!(Sensor::default_for_platform(), expected);
    }

    #[test]
    fn a_sensor_this_platform_lacks_is_refused_by_name() {
        let missing = if cfg!(target_os = "macos") {
            Sensor::Rapl
        } else {
            Sensor::Smc
        };
        let error = Sensor::resolve(Some(missing.name())).expect_err("not available here");
        assert!(error.contains(missing.name()), "{error}");
        assert!(error.contains(std::env::consts::OS), "{error}");
    }

    #[test]
    fn every_sensor_this_platform_has_resolves() {
        for name in Sensor::supported_here() {
            let sensor = Sensor::resolve(Some(name)).expect("supported here");
            assert!(sensor.available_here());
        }
    }

    #[test]
    fn only_powermetrics_asks_for_sudo() {
        assert!(Sensor::Powermetrics.needs_sudo());
        assert!(!Sensor::Smc.needs_sudo());
        assert!(!Sensor::Rapl.needs_sudo());
    }
}
