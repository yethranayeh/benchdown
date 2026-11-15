//! Sampling a power meter in the background.
//!
//! `powermetrics` writes a plist that is parsed after the run. The sensors that
//! read a counter directly (AppleSMC, RAPL) are sampled by a thread instead, and
//! produce the same [`PowerSample`] rows, so the analysis is unchanged.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::samples::PowerSample;
use crate::sensor::Sensor;

/// What one reading of a meter says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pulse {
    /// Instantaneous power in milliwatts.
    #[allow(dead_code)] // constructed by the macOS meter only
    PowerMw(f64),
    /// Energy in millijoules consumed since the previous reading.
    #[allow(dead_code)] // constructed by the Linux meter only
    EnergyMj(f64),
}

/// A counter that can be read repeatedly.
pub trait Meter: Send {
    fn read(&mut self) -> Result<Pulse, String>;

    /// Reads once, so a missing sensor fails before the run starts.
    fn probe(&mut self) -> Result<(), String> {
        self.read().map(|_| ())
    }
}

/// Starts the meter for `sensor`, failing before the run if it cannot read.
pub fn open(sensor: Sensor) -> Result<Box<dyn Meter>, String> {
    match sensor {
        Sensor::Powermetrics => Err("powermetrics is started by the run itself".to_owned()),
        #[cfg(target_os = "macos")]
        Sensor::Smc => Ok(Box::new(SmcMeter::open()?)),
        #[cfg(target_os = "linux")]
        Sensor::Rapl => Ok(Box::new(RaplMeter::open()?)),
        other => Err(format!(
            "sensor {other} is not available on {}",
            std::env::consts::OS
        )),
    }
}

/// Turns one reading into a sample over the window since the previous one.
///
/// The first reading only opens the window. A non-positive window or a power
/// that is not a finite, non-negative number is skipped.
pub(crate) fn record(
    samples: &mut Vec<PowerSample>,
    previous: &mut Option<f64>,
    now: f64,
    pulse: Pulse,
) {
    let Some(start) = *previous else {
        *previous = Some(now);
        return;
    };
    *previous = Some(now);
    let seconds = now - start;
    if seconds <= 0.0 {
        return;
    }
    let power_mw = match pulse {
        Pulse::PowerMw(milliwatts) => milliwatts,
        Pulse::EnergyMj(millijoules) => millijoules / seconds,
    };
    if !power_mw.is_finite() || power_mw < 0.0 {
        return;
    }
    samples.push(PowerSample {
        start_seconds: start,
        end_seconds: now,
        power_mw,
        seconds,
    });
}

/// A meter sampling in the background until stopped.
pub struct Sampler {
    /// The first failure, kept so the run can report why the samples stopped.
    failure: Arc<Mutex<Option<String>>>,
    samples: Arc<Mutex<Vec<PowerSample>>>,
    stop: Mutex<Option<mpsc::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Sampler {
    /// Probes the meter, then samples every `interval_ms` until [`Sampler::stop`].
    pub fn start(mut meter: Box<dyn Meter>, interval_ms: u64) -> Result<Self, String> {
        meter.probe()?;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let failure = Arc::new(Mutex::new(None));
        let (stop, stopped) = mpsc::channel::<()>();
        let interval = Duration::from_millis(interval_ms.max(1));
        let thread_samples = Arc::clone(&samples);
        let thread_failure = Arc::clone(&failure);
        let thread = std::thread::spawn(move || {
            let mut previous = None;
            loop {
                let now = unix_seconds();
                match meter.read() {
                    Ok(pulse) => record(
                        &mut thread_samples.lock().expect("sample lock"),
                        &mut previous,
                        now,
                        pulse,
                    ),
                    Err(error) => {
                        let mut failure = thread_failure.lock().expect("failure lock");
                        if failure.is_none() {
                            *failure = Some(error);
                        }
                    }
                }
                match stopped.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Ok(Self {
            failure,
            samples,
            stop: Mutex::new(Some(stop)),
            thread: Mutex::new(Some(thread)),
        })
    }

    /// The samples taken so far, in time order.
    pub fn samples(&self) -> Vec<PowerSample> {
        self.samples.lock().expect("sample lock").clone()
    }

    /// The first failure the meter reported, if any.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().expect("failure lock").clone()
    }

    /// Stops the thread and waits for it. Safe to call twice.
    pub fn stop(&self) {
        self.stop.lock().expect("stop lock").take();
        if let Some(thread) = self.thread.lock().expect("thread lock").take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// AppleSMC's `PSTR`, whole-system power in watts.
///
/// `PCTR` (processor power) does not exist on Apple Silicon, so there is no
/// processor-only reading; see the task notes on BDN-8.
#[cfg(target_os = "macos")]
pub struct SmcMeter {
    smc: smc::SMC,
}

#[cfg(target_os = "macos")]
impl SmcMeter {
    /// The system power key, in watts.
    const KEY: &'static str = "PSTR";

    pub fn open() -> Result<Self, String> {
        let smc = smc::SMC::new()
            .map_err(|error| format!("cannot open AppleSMC (SMC reads need no sudo): {error}"))?;
        let meter = Self { smc };
        meter.power_mw().map(|_| meter)
    }

    fn power_mw(&self) -> Result<f64, String> {
        let watts: f32 = self
            .smc
            .read_key(Self::KEY.into())
            .map_err(|error| format!("AppleSMC key {}: {error}", Self::KEY))?;
        let milliwatts = f64::from(watts) * 1000.0;
        if !milliwatts.is_finite() || milliwatts <= 0.0 {
            return Err(format!(
                "AppleSMC key {} returned {watts} W, which no Mac reports",
                Self::KEY
            ));
        }
        Ok(milliwatts)
    }
}

#[cfg(target_os = "macos")]
impl Meter for SmcMeter {
    fn read(&mut self) -> Result<Pulse, String> {
        self.power_mw().map(Pulse::PowerMw)
    }
}

/// RAPL package energy, read from the model-specific registers.
#[cfg(target_os = "linux")]
pub struct RaplMeter {
    path: PathBuf,
    energy_msr: u32,
    unit_joules: f64,
    previous_raw: Option<u64>,
}

#[cfg(target_os = "linux")]
impl RaplMeter {
    pub fn open() -> Result<Self, String> {
        let vendor = vendor_from_cpuinfo(
            &std::fs::read_to_string("/proc/cpuinfo")
                .map_err(|error| format!("/proc/cpuinfo: {error}"))?,
        )?;
        let (units_msr, energy_msr) = rapl_msrs(vendor);
        let path = PathBuf::from("/dev/cpu/0/msr");
        let units = read_msr(&path, units_msr)?;
        Ok(Self {
            path,
            energy_msr,
            unit_joules: energy_unit_joules(units),
            previous_raw: None,
        })
    }
}

#[cfg(target_os = "linux")]
impl Meter for RaplMeter {
    fn read(&mut self) -> Result<Pulse, String> {
        let raw = read_msr(&self.path, self.energy_msr)?;
        let millijoules = match self.previous_raw {
            Some(previous) => energy_delta_mj(previous, raw, self.unit_joules),
            None => 0.0,
        };
        self.previous_raw = Some(raw);
        Ok(Pulse::EnergyMj(millijoules))
    }
}

/// The RAPL energy unit exponent, from the power-unit MSR.
///
/// Both Intel and AMD keep it in bits 12:8, as `0.5^bits` joules.
#[allow(dead_code)] // used by the Linux meter, which only builds there
pub(crate) fn energy_unit_joules(units_msr: u64) -> f64 {
    let exponent = (units_msr & 0x1F00) >> 8;
    0.5f64.powi(exponent as i32)
}

/// Energy between two RAPL counter reads, handling the 32-bit wrap.
#[allow(dead_code)] // used by the Linux meter, which only builds there
pub(crate) fn energy_delta_mj(previous: u64, current: u64, unit_joules: f64) -> f64 {
    const WRAP: u64 = 1 << 32;
    let delta = if current >= previous {
        current - previous
    } else {
        WRAP - previous + current
    };
    delta as f64 * unit_joules * 1000.0
}

/// The RAPL vendor, from `/proc/cpuinfo`.
#[allow(dead_code)] // used by the Linux meter, which only builds there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaplVendor {
    Intel,
    Amd,
}

#[allow(dead_code)] // used by the Linux meter, which only builds there
pub(crate) fn vendor_from_cpuinfo(text: &str) -> Result<RaplVendor, String> {
    let vendor = text
        .lines()
        .find_map(|line| line.strip_prefix("vendor_id"))
        .and_then(|rest| rest.split_once(':').map(|(_, value)| value.trim()))
        .ok_or_else(|| "no vendor_id line in /proc/cpuinfo".to_owned())?;
    match vendor {
        "GenuineIntel" => Ok(RaplVendor::Intel),
        "AuthenticAMD" => Ok(RaplVendor::Amd),
        other => Err(format!(
            "{other} has no RAPL counters benchdown knows: Intel and AMD only"
        )),
    }
}

/// The power-unit and package-energy MSR numbers for a vendor.
#[allow(dead_code)] // used by the Linux meter, which only builds there
pub(crate) fn rapl_msrs(vendor: RaplVendor) -> (u32, u32) {
    match vendor {
        RaplVendor::Intel => (0x606, 0x611),
        RaplVendor::Amd => (0xC001_0299, 0xC001_029B),
    }
}

#[cfg(target_os = "linux")]
fn read_msr(path: &std::path::Path, register: u32) -> Result<u64, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|error| {
        format!(
            "{}: {error}. Load the msr module (sudo modprobe msr) and allow reads of \
             /dev/cpu/*/msr",
            path.display()
        )
    })?;
    file.seek(SeekFrom::Start(u64::from(register)))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reading_only_opens_the_window() {
        let mut samples = Vec::new();
        let mut previous = None;
        record(&mut samples, &mut previous, 10.0, Pulse::PowerMw(5_000.0));
        assert!(samples.is_empty());
        assert_eq!(previous, Some(10.0));
    }

    #[test]
    fn instantaneous_power_is_the_reading_over_the_window() {
        let mut samples = Vec::new();
        let mut previous = Some(10.0);
        record(&mut samples, &mut previous, 10.5, Pulse::PowerMw(5_000.0));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].power_mw, 5_000.0);
        assert!((samples[0].seconds - 0.5).abs() < 1e-9);
        assert!((samples[0].start_seconds - 10.0).abs() < 1e-9);
        assert!((samples[0].end_seconds - 10.5).abs() < 1e-9);
    }

    #[test]
    fn cumulative_energy_becomes_power_over_the_window() {
        let mut samples = Vec::new();
        let mut previous = Some(10.0);
        // 1,000 mJ in 0.5 s is 2 W.
        record(&mut samples, &mut previous, 10.5, Pulse::EnergyMj(1_000.0));
        assert_eq!(samples.len(), 1);
        assert!((samples[0].power_mw - 2_000.0).abs() < 1e-9);
    }

    #[test]
    fn a_window_that_did_not_advance_is_skipped() {
        let mut samples = Vec::new();
        let mut previous = Some(10.0);
        record(&mut samples, &mut previous, 10.0, Pulse::PowerMw(5_000.0));
        record(&mut samples, &mut previous, 9.0, Pulse::PowerMw(5_000.0));
        assert!(samples.is_empty());
    }

    #[test]
    fn an_impossible_power_is_skipped() {
        let mut samples = Vec::new();
        for power in [-1.0, f64::NAN, f64::INFINITY] {
            let mut previous = Some(10.0);
            record(&mut samples, &mut previous, 10.5, Pulse::PowerMw(power));
        }
        assert!(samples.is_empty());
    }

    #[test]
    fn the_energy_unit_is_half_to_the_exponent() {
        assert_eq!(energy_unit_joules(0x0000_0000), 1.0);
        assert_eq!(energy_unit_joules(0x0000_0E00), 0.5f64.powi(14));
        // Bits 9:8 are not part of the energy unit and must be ignored.
        assert_eq!(energy_unit_joules(0x0000_0F00), 0.5f64.powi(15));
    }

    #[test]
    fn energy_delta_is_scaled_and_handles_the_wrap() {
        assert!((energy_delta_mj(100, 200, 1.0) - 100_000.0).abs() < 1e-6);
        let wrapped = energy_delta_mj((1 << 32) - 10, 5, 1.0);
        assert!((wrapped - 15_000.0).abs() < 1e-6);
    }

    #[test]
    fn the_vendor_comes_from_the_cpuinfo_line() {
        let intel = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: x\n";
        let amd = "processor\t: 0\nvendor_id\t: AuthenticAMD\n";
        assert_eq!(vendor_from_cpuinfo(intel), Ok(RaplVendor::Intel));
        assert_eq!(vendor_from_cpuinfo(amd), Ok(RaplVendor::Amd));
        let other = vendor_from_cpuinfo("processor\t: 0\n");
        assert!(other.expect_err("no vendor").contains("/proc/cpuinfo"));
        let arm = vendor_from_cpuinfo("vendor_id\t: ARM\n");
        assert!(arm.expect_err("no RAPL").contains("ARM"));
    }

    #[test]
    fn each_vendor_reads_its_own_msrs() {
        assert_eq!(rapl_msrs(RaplVendor::Intel), (0x606, 0x611));
        assert_eq!(rapl_msrs(RaplVendor::Amd), (0xC001_0299, 0xC001_029B));
    }

    #[test]
    fn a_meter_that_fails_on_the_probe_starts_nothing() {
        struct Broken;
        impl Meter for Broken {
            fn read(&mut self) -> Result<Pulse, String> {
                Err("no such key".to_owned())
            }
        }
        let error = match Sampler::start(Box::new(Broken), 1) {
            Ok(_) => panic!("the probe fails"),
            Err(error) => error,
        };
        assert_eq!(error, "no such key");
    }

    #[test]
    fn the_sampler_collects_windows_until_stopped() {
        struct Fixed(f64);
        impl Meter for Fixed {
            fn read(&mut self) -> Result<Pulse, String> {
                Ok(Pulse::PowerMw(self.0))
            }
        }
        let sampler = Sampler::start(Box::new(Fixed(4_000.0)), 5).expect("starts");
        std::thread::sleep(std::time::Duration::from_millis(60));
        sampler.stop();
        let samples = sampler.samples();
        assert!(samples.len() >= 2, "got {} samples", samples.len());
        assert!(samples.iter().all(|sample| sample.power_mw == 4_000.0));
        assert!(sampler.failure().is_none());
    }

    #[test]
    fn stopping_twice_is_safe() {
        struct Fixed;
        impl Meter for Fixed {
            fn read(&mut self) -> Result<Pulse, String> {
                Ok(Pulse::PowerMw(1_000.0))
            }
        }
        let sampler = Sampler::start(Box::new(Fixed), 60_000).expect("starts");
        sampler.stop();
        sampler.stop();
        assert!(sampler.samples().is_empty());
    }

    #[test]
    fn the_first_failure_is_kept() {
        struct Fails(&'static str);
        impl Meter for Fails {
            fn read(&mut self) -> Result<Pulse, String> {
                Err(self.0.to_owned())
            }
            fn probe(&mut self) -> Result<(), String> {
                Ok(())
            }
        }
        let sampler = Sampler::start(Box::new(Fails("sensor went away")), 5).expect("starts");
        std::thread::sleep(std::time::Duration::from_millis(40));
        sampler.stop();
        assert_eq!(sampler.failure().as_deref(), Some("sensor went away"));
        assert!(sampler.samples().is_empty());
    }
}
