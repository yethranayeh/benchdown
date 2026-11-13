//! Reading the samples powermetrics writes with `-f plist`.
//!
//! The analyzer and the live runner land in later commits; until then these
//! functions are reached by the tests only.

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

const NANOSECONDS_PER_SECOND: f64 = 1e9;

/// One usable sample from a power log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerSample {
    /// Unix seconds when the sample's interval starts.
    pub start_seconds: f64,
    /// Unix seconds when the sample's interval ends.
    pub end_seconds: f64,
    /// Estimated CPU, GPU and ANE power in milliwatts.
    pub power_mw: f64,
    /// The interval length in seconds.
    pub seconds: f64,
}

/// Splits the NUL-separated plist stream and keeps the chunks that parse.
pub fn parse_records(text: &str) -> Vec<plist::Value> {
    text.split('\0')
        .filter(|chunk| !chunk.trim().is_empty())
        .filter_map(|chunk| plist::Value::from_reader_xml(chunk.as_bytes()).ok())
        .collect()
}

/// Splits the NUL-separated plist stream and keeps usable samples.
///
/// A sample is skipped when its plist does not parse, when `elapsed_ns`,
/// `timestamp` or `processor.combined_power` is missing or not a number, and
/// when `elapsed_ns` is not positive. The final chunk is often cut off when
/// powermetrics stops; it does not parse and is skipped.
pub fn parse_power_log(text: &str) -> Vec<PowerSample> {
    parse_records(text)
        .iter()
        .filter_map(power_sample)
        .collect()
}

fn power_sample(value: &plist::Value) -> Option<PowerSample> {
    let record = value.as_dictionary()?;
    let processor = record.get("processor")?.as_dictionary()?;

    let power_mw = number(processor.get("combined_power")?)?;
    let elapsed_ns = number(record.get("elapsed_ns")?)?;
    let timestamp = record.get("timestamp")?.as_date()?;
    if elapsed_ns <= 0.0 {
        return None;
    }

    let seconds = elapsed_ns / NANOSECONDS_PER_SECOND;
    let end_seconds = SystemTime::from(timestamp)
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();

    Some(PowerSample {
        start_seconds: end_seconds - seconds,
        end_seconds,
        power_mw,
        seconds,
    })
}

/// A plist number, whether it was written as an integer or a real.
pub(crate) fn number(value: &plist::Value) -> Option<f64> {
    value.as_real().or_else(|| {
        value
            .as_signed_integer()
            .map(|number| number as f64)
            .or_else(|| value.as_unsigned_integer().map(|number| number as f64))
    })
}

/// Samples whose whole interval lies inside the window.
pub fn samples_inside(
    samples: &[PowerSample],
    start_seconds: f64,
    end_seconds: f64,
) -> Vec<PowerSample> {
    samples
        .iter()
        .copied()
        .filter(|sample| sample.start_seconds >= start_seconds && sample.end_seconds <= end_seconds)
        .collect()
}

/// Duration-weighted mean power in milliwatts, or `None` when no sample falls inside.
pub fn mean_power_mw(samples: &[PowerSample], start_seconds: f64, end_seconds: f64) -> Option<f64> {
    let inside = samples_inside(samples, start_seconds, end_seconds);
    if inside.is_empty() {
        return None;
    }

    let mut energy = 0.0;
    let mut seconds = 0.0;
    for sample in &inside {
        energy += sample.power_mw * sample.seconds;
        seconds += sample.seconds;
    }
    Some(energy / seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::parse_blocks;

    const POWER_LOG: &str = include_str!("../tests/fixtures/power.plist");
    const BLOCK_LOG: &str = include_str!("../tests/fixtures/blocks.tsv");

    /// The fixture timestamps start at 2026-11-08T10:00:00Z.
    const BASE_SECONDS: f64 = 1_794_132_000.0;

    #[test]
    fn a_log_keeps_its_twelve_usable_samples() {
        let samples = parse_power_log(POWER_LOG);
        assert_eq!(samples.len(), 12);
    }

    #[test]
    fn a_sample_window_ends_at_its_timestamp() {
        let samples = parse_power_log(POWER_LOG);
        let first = samples[0];
        assert_eq!(first.end_seconds, BASE_SECONDS + 1.0);
        assert_eq!(first.seconds, 1.0);
        assert_eq!(first.start_seconds, BASE_SECONDS);
        assert_eq!(first.power_mw, 100.0);
        assert_eq!(samples[11].power_mw, 210.0);
    }

    #[test]
    fn broken_samples_are_skipped() {
        // The fixture ends with a sample that has no combined_power, one whose
        // elapsed_ns is 0 and one cut off before its closing tags.
        let samples = parse_power_log(POWER_LOG);
        assert!(samples.iter().all(|sample| sample.power_mw != 999.0));
        assert!(samples.iter().all(|sample| sample.power_mw != 220.0));
    }

    #[test]
    fn only_samples_fully_inside_a_window_count() {
        let samples = parse_power_log(POWER_LOG);
        let inside = samples_inside(&samples, BASE_SECONDS, BASE_SECONDS + 4.0);
        assert_eq!(inside.len(), 4);
        assert_eq!(
            inside
                .iter()
                .map(|sample| sample.power_mw)
                .collect::<Vec<_>>(),
            [100.0, 110.0, 120.0, 130.0]
        );
    }

    #[test]
    fn the_mean_is_weighted_by_sample_length() {
        let samples = parse_power_log(POWER_LOG);
        let idle = mean_power_mw(&samples, BASE_SECONDS, BASE_SECONDS + 4.0);
        assert_eq!(idle, Some(115.0));
        let work = mean_power_mw(&samples, BASE_SECONDS + 4.0, BASE_SECONDS + 8.0);
        assert_eq!(work, Some(155.0));
    }

    #[test]
    fn a_window_without_samples_has_no_mean() {
        let samples = parse_power_log(POWER_LOG);
        assert_eq!(mean_power_mw(&samples, BASE_SECONDS, BASE_SECONDS), None);
        assert_eq!(
            mean_power_mw(&samples, BASE_SECONDS - 100.0, BASE_SECONDS - 50.0),
            None
        );
    }

    #[test]
    fn the_fixture_block_log_agrees_with_the_fixture_samples() {
        let samples = parse_power_log(POWER_LOG);
        let blocks = parse_blocks(BLOCK_LOG).expect("the fixture block log parses");
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            mean_power_mw(&samples, blocks[0].start, blocks[0].end),
            Some(115.0)
        );
        assert_eq!(
            mean_power_mw(&samples, blocks[1].start, blocks[1].end),
            Some(155.0)
        );
    }

    #[test]
    fn a_sample_may_carry_integers_arrays_booleans_and_dates() {
        let chunk = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<plist version=\"1.0\">
<dict>
<key>is_delta</key><true/>
<key>elapsed_ns</key><integer>250000000</integer>
<key>timestamp</key><date>2026-11-08T10:00:00Z</date>
<key>processor</key>
<dict>
<key>clusters</key><array><dict><key>freq_hz</key><real>1.5</real></dict></array>
<key>combined_power</key><integer>90</integer>
</dict>
</dict>
</plist>";
        let samples = parse_power_log(chunk);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].power_mw, 90.0);
        assert_eq!(samples[0].seconds, 0.25);
        assert_eq!(samples[0].end_seconds, BASE_SECONDS);
        assert_eq!(samples[0].start_seconds, BASE_SECONDS - 0.25);
    }

    // The recorded runs stay outside the repo. These tests read them from the
    // paths the environment names, and skip when nothing is there.

    use std::path::Path;
    use std::time::Instant;

    fn read(path: &Path) -> Option<String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) => {
                eprintln!("skipping: {} could not be read ({error})", path.display());
                None
            }
        }
    }

    fn median(values: &mut [f64]) -> f64 {
        values.sort_by(f64::total_cmp);
        let middle = values.len() / 2;
        if values.len() % 2 == 1 {
            values[middle]
        } else {
            (values[middle - 1] + values[middle]) / 2.0
        }
    }

    fn recorded_value(json: &str, key: &str) -> f64 {
        let needle = format!("\"{key}\":");
        let start = json
            .find(&needle)
            .unwrap_or_else(|| panic!("{key} missing from analysis.json"))
            + needle.len();
        let number: String = json[start..]
            .trim_start()
            .chars()
            .take_while(|character| {
                character.is_ascii_digit() || matches!(character, '.' | '-' | 'e' | 'E' | '+')
            })
            .collect();
        number
            .parse()
            .unwrap_or_else(|_| panic!("{key} is not a number in analysis.json: {number}"))
    }

    #[test]
    #[ignore = "reads the recorded run named by BENCHDOWN_REFERENCE_A"]
    fn recorded_run_a_still_parses() {
        let Some(directory) = std::env::var_os("BENCHDOWN_REFERENCE_A") else {
            eprintln!(
                "skipping: set BENCHDOWN_REFERENCE_A to a run directory holding \
                 power.plist, blocks.tsv and analysis.json"
            );
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let Some(power_log) = read(&directory.join("power.plist")) else {
            return;
        };

        let started = Instant::now();
        let samples = parse_power_log(&power_log);
        let elapsed = started.elapsed();
        println!("run A: {} samples parsed in {elapsed:.2?}", samples.len());
        assert_eq!(samples.len(), 2_671);

        let Some(block_log) = read(&directory.join("blocks.tsv")) else {
            return;
        };
        let blocks = parse_blocks(&block_log).expect("run A's block log parses");
        println!("run A: {} block rows", blocks.len());
        assert_eq!(blocks.len(), 60);

        let Some(analysis) = read(&directory.join("analysis.json")) else {
            return;
        };
        assert_eq!(
            recorded_value(&analysis, "sampleCount") as usize,
            samples.len()
        );
        let mut seconds: Vec<f64> = samples.iter().map(|sample| sample.seconds).collect();
        let recorded = recorded_value(&analysis, "medianIntervalSeconds");
        let measured = median(&mut seconds);
        assert!(
            (measured - recorded).abs() < 1e-9,
            "median interval {measured} differs from the recorded {recorded}"
        );
        println!("run A: median interval {measured} s");
    }

    #[test]
    #[ignore = "reads the recorded run named by BENCHDOWN_REFERENCE_B"]
    fn recorded_run_b_still_parses() {
        let Some(path) = std::env::var_os("BENCHDOWN_REFERENCE_B") else {
            eprintln!(
                "skipping: set BENCHDOWN_REFERENCE_B to a recorded power plist, \
                 or to a directory holding power.plist, beside its blocks.tsv"
            );
            return;
        };
        let path = std::path::PathBuf::from(path);
        let path = if path.is_dir() {
            path.join("power.plist")
        } else {
            path
        };
        let Some(power_log) = read(&path) else {
            return;
        };

        let started = Instant::now();
        let samples = parse_power_log(&power_log);
        let elapsed = started.elapsed();
        println!("run B: {} samples parsed in {elapsed:.2?}", samples.len());
        assert!(!samples.is_empty());

        if let Some(block_log) = read(&path.with_extension("tsv")) {
            let blocks = parse_blocks(&block_log).expect("run B's block log parses");
            println!("run B: {} block rows", blocks.len());
            assert!(!blocks.is_empty());
        }
    }
}
