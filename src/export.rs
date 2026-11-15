//! Hyperfine-shaped JSON results.
//!
//! The envelope follows hyperfine 2.0.0's `--export-json` (schema version 2):
//! metadata, one result per command with per-round measurements and a summary
//! map, and a primary metric. benchdown adds the machine block, the idle
//! baseline and its clean-round verdicts around that.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::analysis::{Analysis, BlockAnalysis, CleanVerdict, DropReason, Verdict};

use crate::blocks::BlockKind;
use crate::gate::ContaminationReason;

/// The metric this file reports.
pub const PRIMARY_METRIC: &str = "energy_per_run";

/// The hyperfine JSON schema this envelope follows.
pub const SCHEMA_VERSION: u32 = 2;

/// The unit every energy value uses.
pub const ENERGY_UNIT: &str = "millijoule";

/// The sensor that recorded the samples.
pub const SENSOR: &str = "powermetrics";

/// What every result measured, in one line.
pub const STATEMENT: &str =
    "Whole-machine processor energy above an idle baseline, not the energy of one process.";

/// Everything the file holds.
#[derive(Debug, Serialize)]
pub struct Export {
    schema_version: u32,
    metadata: Metadata,
    primary_metric: &'static str,
    method: Method,
    idle_baseline: IdleBaseline,
    results: Vec<ResultEntry>,
}

#[derive(Debug, Serialize)]
struct Metadata {
    generator: &'static str,
    benchdown_version: &'static str,
    start_time: String,
    platform: Platform,
}

#[derive(Debug, Serialize)]
struct Platform {
    os: String,
    architecture: String,
}

#[derive(Debug, Serialize)]
struct Method {
    sensor: &'static str,
    os_build: String,
    hardware_model: String,
    sample_count: usize,
    median_interval_seconds: f64,
    statement: &'static str,
}

#[derive(Debug, Serialize)]
struct IdleBaseline {
    unit: &'static str,
    median: f64,
    min: f64,
    max: f64,
}

#[derive(Debug, Serialize)]
struct ResultEntry {
    command: String,
    name: String,
    measurements: Vec<Measurement>,
    summary: BTreeMap<&'static str, Summary>,
    runs_per_block: u64,
    spread: f64,
    verdict: Verdict,
    clean_rounds: usize,
    clean_verdict: CleanVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    clean_summary: Option<BTreeMap<&'static str, Summary>>,
    dropped_rounds: Vec<DroppedRound>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    round: u64,
    energy_per_run: Quantity,
    /// Every measured run exited 0; a non-zero exit stops the run before analysis.
    exit_code: i32,
}

#[derive(Debug, Serialize)]
struct Quantity {
    value: f64,
    unit: &'static str,
}

#[derive(Debug, Serialize)]
struct Summary {
    unit: &'static str,
    count: usize,
    mean: f64,
    stddev: Option<f64>,
    median: f64,
    min: f64,
    max: f64,
}

/// Why a round was dropped, in the export's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DroppedReason {
    Idle,
    Work,
    Load,
    Process,
}

impl DroppedReason {
    fn order(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Work => 1,
            Self::Load => 2,
            Self::Process => 3,
        }
    }
}

#[derive(Debug, Serialize)]
struct DroppedRound {
    round: u64,
    reason: DroppedReason,
    /// `milliwatt` for the idle and work limits, `load` or `percent` for contamination.
    unit: &'static str,
    observed: f64,
    limit: f64,
    /// The background process, for a process reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    process: Option<String>,
}

/// What a live run knows that the analysis does not.
pub struct RunInfo<'a> {
    /// Each command's name and command string, in command-line order.
    pub workloads: &'a [(String, String)],
    pub started_at: &'a str,
    pub os_build: &'a str,
    pub hardware_model: &'a str,
    pub os: &'a str,
    pub architecture: &'a str,
}

/// Builds the export from an analysis and the run's machine details.
pub fn build(analysis: &Analysis, run: RunInfo<'_>) -> Result<Export, String> {
    let mut results = Vec::new();

    for (name, command) in run.workloads {
        let workload = analysis
            .workloads
            .iter()
            .find(|workload| &workload.label == name)
            .ok_or_else(|| format!("{name} has no measured round, so it cannot be exported"))?;

        let mut rounds: Vec<&BlockAnalysis> = analysis
            .blocks
            .iter()
            .filter(|block| {
                block.kind == BlockKind::Work && &block.label == name && block.per_run_mj.is_some()
            })
            .collect();
        rounds.sort_by_key(|block| block.round);

        let values: Vec<f64> = rounds.iter().filter_map(|block| block.per_run_mj).collect();
        let clean: Vec<f64> = rounds
            .iter()
            .filter(|block| !block.dropped)
            .filter_map(|block| block.per_run_mj)
            .collect();

        results.push(ResultEntry {
            command: command.clone(),
            name: name.clone(),
            measurements: rounds
                .iter()
                .map(|block| Measurement {
                    round: block.round,
                    energy_per_run: Quantity {
                        value: block.per_run_mj.expect("the round has a per-run figure"),
                        unit: ENERGY_UNIT,
                    },
                    exit_code: 0,
                })
                .collect(),
            summary: summary_map(&values),
            runs_per_block: workload.runs_per_block,
            spread: workload.spread,
            verdict: workload.verdict,
            clean_rounds: workload.clean_rounds,
            clean_verdict: workload.clean_verdict,
            clean_summary: (!clean.is_empty()).then(|| summary_map(&clean)),
            dropped_rounds: dropped_rounds(analysis, name),
        });
    }

    Ok(Export {
        schema_version: SCHEMA_VERSION,
        metadata: Metadata {
            generator: env!("CARGO_PKG_NAME"),
            benchdown_version: env!("CARGO_PKG_VERSION"),
            start_time: run.started_at.to_owned(),
            platform: Platform {
                os: run.os.to_owned(),
                architecture: run.architecture.to_owned(),
            },
        },
        primary_metric: PRIMARY_METRIC,
        method: Method {
            sensor: SENSOR,
            os_build: run.os_build.to_owned(),
            hardware_model: run.hardware_model.to_owned(),
            sample_count: analysis.sample_count,
            median_interval_seconds: analysis.median_interval_seconds,
            statement: STATEMENT,
        },
        idle_baseline: IdleBaseline {
            unit: "milliwatt",
            median: analysis.idle_median_mw,
            min: analysis.idle_min_mw,
            max: analysis.idle_max_mw,
        },
        results,
    })
}

/// Writes the export as indented JSON with a trailing newline.
pub fn write(path: &Path, export: &Export) -> Result<(), String> {
    let text = serde_json::to_string_pretty(export).map_err(|error| error.to_string())?;
    std::fs::write(path, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn summary_map(values: &[f64]) -> BTreeMap<&'static str, Summary> {
    BTreeMap::from([(PRIMARY_METRIC, Summary::new(values))])
}

impl Summary {
    fn new(values: &[f64]) -> Self {
        assert!(!values.is_empty(), "a summary needs at least one round");
        let count = values.len();
        let mean = values.iter().sum::<f64>() / count as f64;
        // Sample standard deviation, as hyperfine reports it.
        let stddev = (count > 1).then(|| {
            let squared: f64 = values
                .iter()
                .map(|value| (value - mean) * (value - mean))
                .sum();
            (squared / (count - 1) as f64).sqrt()
        });

        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let middle = count / 2;
        let median = if count % 2 == 1 {
            sorted[middle]
        } else {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        };

        Self {
            unit: ENERGY_UNIT,
            count,
            mean,
            stddev,
            median,
            min: sorted[0],
            max: sorted[count - 1],
        }
    }
}

/// One entry per reason that dropped one of this result's rounds.
///
/// The round is global to the schedule, so an idle spike on another command in
/// the same round still leaves this result's clean summary without it; that entry
/// is kept here. Contamination reasons are added beside an idle or work reason,
/// never instead of it.
fn dropped_rounds(analysis: &Analysis, label: &str) -> Vec<DroppedRound> {
    let mut rounds: Vec<u64> = analysis
        .blocks
        .iter()
        .filter(|block| {
            block.kind == BlockKind::Work
                && block.label.as_str() == label
                && block.per_run_mj.is_some()
        })
        .map(|block| block.round)
        .collect();
    rounds.sort_unstable();
    rounds.dedup();

    let mut entries: Vec<DroppedRound> = Vec::new();
    for round in rounds {
        if let Some(dropped) = analysis
            .dropped_blocks
            .iter()
            .find(|dropped| dropped.round == round)
        {
            entries.push(DroppedRound {
                round,
                reason: match dropped.reason {
                    DropReason::Idle => DroppedReason::Idle,
                    DropReason::Work => DroppedReason::Work,
                },
                unit: "milliwatt",
                observed: dropped.observed_mw,
                limit: dropped.limit_mw,
                process: None,
            });
        }

        let mut seen: Vec<ContaminationReason> = Vec::new();
        for contaminated in analysis
            .contaminated_blocks
            .iter()
            .filter(|contaminated| contaminated.round == round)
        {
            if seen.contains(&contaminated.reason) {
                continue;
            }
            seen.push(contaminated.reason);
            entries.push(DroppedRound {
                round,
                reason: match contaminated.reason {
                    ContaminationReason::Load => DroppedReason::Load,
                    ContaminationReason::Process => DroppedReason::Process,
                },
                unit: match contaminated.reason {
                    ContaminationReason::Load => "load",
                    ContaminationReason::Process => "percent",
                },
                observed: contaminated.observed,
                limit: contaminated.limit,
                process: contaminated.process.clone(),
            });
        }
    }
    entries.sort_by_key(|entry| (entry.round, entry.reason.order()));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{
        Analysis, BlockAnalysis, CleanVerdict, DroppedBlock, Verdict, WorkloadAnalysis, analyze,
    };
    use crate::blocks::parse_blocks;
    use crate::samples::parse_power_log;
    use serde_json::Value;

    const POWER: &str = include_str!("../tests/fixtures/power.plist");
    const BLOCKS: &str = include_str!("../tests/fixtures/blocks.tsv");

    fn fixture_analysis() -> Analysis {
        let samples = parse_power_log(POWER);
        let blocks = parse_blocks(BLOCKS).expect("the fixture block log parses");
        analyze(&samples, &blocks, &[], crate::analysis::Limits::default())
            .expect("the fixture analyzes")
    }

    fn fixture_export() -> Value {
        fixture_export_for(&[("one".to_owned(), "echo one".to_owned())])
    }

    fn fixture_export_for(workloads: &[(String, String)]) -> Value {
        let analysis = fixture_analysis();
        let export = build(
            &analysis,
            RunInfo {
                workloads,
                started_at: "2026-11-11T23:10:47Z",
                os_build: "24G517",
                hardware_model: "Mac16,1",
                os: "Darwin",
                architecture: "arm64",
            },
        )
        .expect("the fixture exports");
        serde_json::to_value(&export).expect("the export serializes")
    }

    fn number(value: &Value) -> f64 {
        value.as_f64().expect("a number")
    }

    #[test]
    fn the_export_has_the_hyperfine_envelope() {
        let json = fixture_export();

        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["metadata"]["generator"], "benchdown");
        assert_eq!(
            json["metadata"]["benchdown_version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(json["metadata"]["start_time"], "2026-11-11T23:10:47Z");
        assert_eq!(json["metadata"]["platform"]["os"], "Darwin");
        assert_eq!(json["metadata"]["platform"]["architecture"], "arm64");
        assert_eq!(json["primary_metric"], "energy_per_run");

        assert_eq!(json["method"]["sensor"], "powermetrics");
        assert_eq!(json["method"]["os_build"], "24G517");
        assert_eq!(json["method"]["hardware_model"], "Mac16,1");
        assert_eq!(json["method"]["sample_count"], 12);
        assert_eq!(number(&json["method"]["median_interval_seconds"]), 1.0);
        assert_eq!(json["method"]["statement"], STATEMENT);

        assert_eq!(json["idle_baseline"]["unit"], "milliwatt");
        assert_eq!(number(&json["idle_baseline"]["median"]), 115.0);
        assert_eq!(number(&json["idle_baseline"]["min"]), 115.0);
        assert_eq!(number(&json["idle_baseline"]["max"]), 115.0);
    }

    #[test]
    fn a_result_reports_each_round_and_its_summary() {
        let json = fixture_export();
        let result = &json["results"][0];

        assert_eq!(result["command"], "echo one");
        assert_eq!(result["name"], "one");
        assert_eq!(result["runs_per_block"], 120);
        assert_eq!(number(&result["spread"]), 0.0);
        assert_eq!(result["verdict"], "ok");
        assert_eq!(result["clean_rounds"], 1);
        assert_eq!(result["clean_verdict"], "rerun");
        assert_eq!(result["dropped_rounds"], serde_json::json!([]));

        let measurement = &result["measurements"][0];
        assert_eq!(measurement["round"], 1);
        assert_eq!(measurement["exit_code"], 0);
        assert_eq!(measurement["energy_per_run"]["unit"], "millijoule");
        assert!((number(&measurement["energy_per_run"]["value"]) - 4.0 / 3.0).abs() < 1e-12);

        let summary = &result["summary"]["energy_per_run"];
        assert_eq!(summary["unit"], "millijoule");
        assert_eq!(summary["count"], 1);
        assert!((number(&summary["mean"]) - 4.0 / 3.0).abs() < 1e-12);
        assert!(summary["stddev"].is_null(), "one round has no stddev");
        assert!((number(&summary["median"]) - 4.0 / 3.0).abs() < 1e-12);
        assert!((number(&summary["min"]) - 4.0 / 3.0).abs() < 1e-12);
        assert!((number(&summary["max"]) - 4.0 / 3.0).abs() < 1e-12);

        let clean = &result["clean_summary"]["energy_per_run"];
        assert_eq!(clean["count"], 1);
        assert!((number(&clean["median"]) - 4.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn dropped_rounds_keep_their_measurement_and_leave_the_clean_summary() {
        let analysis = hand_built_analysis();
        let workloads = [("x".to_owned(), "echo x".to_owned())];
        let export = build(
            &analysis,
            RunInfo {
                workloads: &workloads,
                started_at: "2026-11-11T23:10:47Z",
                os_build: "24G517",
                hardware_model: "Mac16,1",
                os: "Darwin",
                architecture: "arm64",
            },
        )
        .expect("the analysis exports");
        let json = serde_json::to_value(&export).expect("the export serializes");
        let result = &json["results"][0];

        let rounds: Vec<u64> = result["measurements"]
            .as_array()
            .expect("measurements is an array")
            .iter()
            .map(|measurement| measurement["round"].as_u64().expect("a round number"))
            .collect();
        assert_eq!(rounds, [1, 2, 3], "every round is kept, dropped or not");

        let summary = &result["summary"]["energy_per_run"];
        assert_eq!(summary["count"], 3);
        assert!((number(&summary["mean"]) - 4.0).abs() < 1e-12);
        assert!((number(&summary["stddev"]) - 2.0).abs() < 1e-12);

        let clean = &result["clean_summary"]["energy_per_run"];
        assert_eq!(clean["count"], 2);
        assert!((number(&clean["mean"]) - 5.0).abs() < 1e-12);
        assert!((number(&clean["stddev"]) - 2.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(result["clean_rounds"], 2);
        assert_eq!(result["clean_verdict"], "rerun");

        let dropped = result["dropped_rounds"]
            .as_array()
            .expect("dropped rounds is an array");
        assert_eq!(dropped.len(), 1, "one entry per round, not per block");
        assert_eq!(dropped[0]["round"], 1);
        assert_eq!(dropped[0]["reason"], "idle");
        assert_eq!(dropped[0]["unit"], "milliwatt");
        assert_eq!(number(&dropped[0]["observed"]), 1962.0);
        assert_eq!(number(&dropped[0]["limit"]), 1000.0);
    }

    #[test]
    fn a_contaminated_round_reports_its_own_reason_beside_the_idle_one() {
        let mut analysis = hand_built_analysis();
        analysis.contaminated_blocks = vec![
            crate::gate::ContaminatedBlock {
                round: 1,
                label: "x".to_owned(),
                kind: BlockKind::Work,
                reason: ContaminationReason::Load,
                observed: 4.4,
                limit: 3.5,
                process: None,
            },
            crate::gate::ContaminatedBlock {
                round: 1,
                label: "x".to_owned(),
                kind: BlockKind::Work,
                reason: ContaminationReason::Process,
                observed: 42.0,
                limit: 20.0,
                process: Some("mds_stores".to_owned()),
            },
        ];
        let workloads = [("x".to_owned(), "echo x".to_owned())];
        let export = build(
            &analysis,
            RunInfo {
                workloads: &workloads,
                started_at: "2026-11-11T23:10:47Z",
                os_build: "24G517",
                hardware_model: "Mac16,1",
                os: "Darwin",
                architecture: "arm64",
            },
        )
        .expect("the analysis exports");
        let json = serde_json::to_value(&export).expect("the export serializes");
        let dropped = json["results"][0]["dropped_rounds"]
            .as_array()
            .expect("dropped rounds is an array");

        assert_eq!(dropped.len(), 3, "idle, load and process all survive");
        assert_eq!(dropped[0]["reason"], "idle");
        assert_eq!(dropped[1]["reason"], "load");
        assert_eq!(dropped[1]["unit"], "load");
        assert_eq!(number(&dropped[1]["observed"]), 4.4);
        assert_eq!(number(&dropped[1]["limit"]), 3.5);
        assert!(dropped[1].get("process").is_none());
        assert_eq!(dropped[2]["reason"], "process");
        assert_eq!(dropped[2]["unit"], "percent");
        assert_eq!(number(&dropped[2]["observed"]), 42.0);
        assert_eq!(number(&dropped[2]["limit"]), 20.0);
        assert_eq!(dropped[2]["process"], "mds_stores");
    }

    #[test]
    fn a_command_without_a_measured_round_cannot_be_exported() {
        let analysis = fixture_analysis();
        let workloads = [("missing".to_owned(), "echo missing".to_owned())];
        let error = build(
            &analysis,
            RunInfo {
                workloads: &workloads,
                started_at: "2026-11-11T23:10:47Z",
                os_build: "24G517",
                hardware_model: "Mac16,1",
                os: "Darwin",
                architecture: "arm64",
            },
        )
        .expect_err("nothing measured that command");

        assert!(error.contains("missing"), "{error}");
        assert!(error.contains("cannot be exported"), "{error}");
    }

    #[test]
    fn the_summary_uses_the_sample_standard_deviation() {
        let summary = Summary::new(&[2.0, 4.0, 6.0]);
        assert_eq!(summary.count, 3);
        assert_eq!(summary.mean, 4.0);
        assert_eq!(summary.stddev, Some(2.0));
        assert_eq!(summary.median, 4.0);
        assert_eq!(summary.min, 2.0);
        assert_eq!(summary.max, 6.0);
        assert_eq!(Summary::new(&[5.0]).stddev, None);
    }

    #[test]
    fn a_round_dropped_for_another_command_still_names_its_reason() {
        let mut analysis = hand_built_analysis();
        analysis.dropped_blocks[0].label = "other".to_owned();
        let workloads = [("x".to_owned(), "echo x".to_owned())];
        let export = build(
            &analysis,
            RunInfo {
                workloads: &workloads,
                started_at: "2026-11-11T23:10:47Z",
                os_build: "24G517",
                hardware_model: "Mac16,1",
                os: "Darwin",
                architecture: "arm64",
            },
        )
        .expect("the analysis exports");
        let json = serde_json::to_value(&export).expect("the export serializes");
        let dropped = json["results"][0]["dropped_rounds"]
            .as_array()
            .expect("dropped rounds is an array");

        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0]["round"], 1);
        assert_eq!(dropped[0]["reason"], "idle");
    }

    fn hand_built_analysis() -> Analysis {
        let block = |round: u64, dropped: bool, per_run_mj: f64| BlockAnalysis {
            round,
            label: "x".to_owned(),
            kind: BlockKind::Work,
            start: 0.0,
            end: 5.0,
            count: 10,
            dropped,
            average_mw: Some(1_000.0),
            skipped: None,
            idle_mw: Some(100.0),
            work_seconds: Some(5.0),
            work_mj: Some(per_run_mj * 10.0 + 500.0),
            net_mj: Some(per_run_mj * 10.0),
            per_run_mj: Some(per_run_mj),
            drop_reason: dropped.then_some(DropReason::Idle),
            contamination: None,
        };

        Analysis {
            sample_count: 30,
            median_interval_seconds: 0.5,
            idle_median_mw: 100.0,
            idle_min_mw: 90.0,
            idle_max_mw: 110.0,
            blocks: vec![
                block(1, true, 2.0),
                block(2, false, 4.0),
                block(3, false, 6.0),
            ],
            workloads: vec![WorkloadAnalysis {
                label: "x".to_owned(),
                runs_per_block: 10,
                median_mj: 4.0,
                min_mj: 2.0,
                max_mj: 6.0,
                median_micro_watt_hours: 4.0 / 3.6,
                spread: 1.0,
                verdict: Verdict::Noisy,
                clean_median_mj: Some(5.0),
                clean_min_mj: Some(4.0),
                clean_max_mj: Some(6.0),
                clean_spread: Some(0.4),
                clean_rounds: 2,
                clean_verdict: CleanVerdict::Rerun,
            }],
            dropped_blocks: vec![
                DroppedBlock {
                    round: 1,
                    label: "x".to_owned(),
                    kind: BlockKind::Idle,
                    reason: DropReason::Idle,
                    observed_mw: 1_962.0,
                    limit_mw: 1_000.0,
                },
                DroppedBlock {
                    round: 1,
                    label: "x".to_owned(),
                    kind: BlockKind::Work,
                    reason: DropReason::Work,
                    observed_mw: 2_500.0,
                    limit_mw: 1_800.0,
                },
            ],
            contaminated_blocks: Vec::new(),
            dropped_rounds: vec![1],
        }
    }
}
