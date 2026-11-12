//! Energy per run for each workload, from samples and block rows.

use serde::Serialize;

use crate::blocks::{BlockKind, BlockRow};
use crate::samples::{PowerSample, mean_power_mw, samples_inside};

/// Spread above this fraction means the figure is not stated.
pub const NOISY_SPREAD: f64 = 0.5;

/// An idle block above this many milliwatts marks its round as disturbed.
pub const MAXIMUM_IDLE_MW: f64 = 1_000.0;

/// A work block above this multiple of its workload's median marks its round as disturbed.
pub const WORK_SPIKE_FACTOR: f64 = 1.5;

/// Fewer clean rounds than this leaves the workload marked for a rerun.
pub const MINIMUM_CLEAN_ROUNDS: usize = 4;

const MILLIJOULES_PER_MICRO_WATT_HOUR: f64 = 3.6;
const PERCENT: f64 = 100.0;
const DECIMAL_PLACES: usize = 1;

/// Why a round was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DropReason {
    Idle,
    Work,
}

/// A block that disturbed its round, with what was measured and the limit it broke.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedBlock {
    pub round: u64,
    pub label: String,
    pub kind: BlockKind,
    pub reason: DropReason,
    pub observed_mw: f64,
    pub limit_mw: f64,
}

/// One block, its window and whatever energy it could be given.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockAnalysis {
    pub round: u64,
    pub label: String,
    pub kind: BlockKind,
    pub start: f64,
    pub end: f64,
    pub count: u64,
    pub dropped: bool,
    /// Duration-weighted power over this block's own window, when it has samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_mw: Option<f64>,
    /// Why this block could not be paired with samples, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Work blocks only: the paired idle block's duration-weighted power.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_mw: Option<f64>,
    /// Work blocks only: seconds of samples that fell inside the block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_seconds: Option<f64>,
    /// Work blocks only: energy over the counted work samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_run_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_reason: Option<DropReason>,
}

/// One command's energy across the rounds that measured it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadAnalysis {
    pub label: String,
    pub runs_per_block: u64,
    pub median_mj: f64,
    pub min_mj: f64,
    pub max_mj: f64,
    pub median_micro_watt_hours: f64,
    pub spread: f64,
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_median_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_min_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_max_mj: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_spread: Option<f64>,
    pub clean_rounds: usize,
    pub clean_verdict: CleanVerdict,
}

/// Whether a workload's spread is small enough to state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    Noisy,
}

/// Whether enough clean rounds remain for the workload to stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanVerdict {
    Ok,
    Rerun,
}

/// Everything one power log and block log say about their run.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub sample_count: usize,
    pub median_interval_seconds: f64,
    pub idle_median_mw: f64,
    pub idle_min_mw: f64,
    pub idle_max_mw: f64,
    pub blocks: Vec<BlockAnalysis>,
    pub workloads: Vec<WorkloadAnalysis>,
    pub dropped_blocks: Vec<DroppedBlock>,
    pub dropped_rounds: Vec<u64>,
}

/// A work block paired with its idle block, with the energy between them.
struct WorkPair {
    block_index: usize,
    block: BlockRow,
    idle_mw: f64,
    work_average_mw: f64,
    work_seconds: f64,
    work_mj: f64,
    net_mj: f64,
    per_run_mj: f64,
}

/// Turns samples and block rows into energy per run, dropping disturbed rounds.
pub fn analyze(samples: &[PowerSample], blocks: &[BlockRow]) -> Result<Analysis, String> {
    if samples.is_empty() {
        return Err("no usable samples: processor.combined_power missing from the log".to_owned());
    }

    let mut analyses: Vec<BlockAnalysis> = blocks.iter().map(base_analysis).collect();
    let pairs = pair_blocks(samples, blocks, &mut analyses)?;
    if pairs.is_empty() {
        return Err("no work block has samples: nothing to analyze".to_owned());
    }
    let idle_powers: Vec<f64> = pairs.iter().map(|pair| pair.idle_mw).collect();

    for pair in &pairs {
        let analysis = &mut analyses[pair.block_index];
        analysis.average_mw = Some(pair.work_average_mw);
        analysis.idle_mw = Some(pair.idle_mw);
        analysis.work_seconds = Some(pair.work_seconds);
        analysis.work_mj = Some(pair.work_mj);
        analysis.net_mj = Some(pair.net_mj);
        analysis.per_run_mj = Some(pair.per_run_mj);
    }

    let workload_medians = median_work_power(&pairs);
    let dropped = find_dropped_blocks(blocks, &analyses, &workload_medians);
    let dropped_rounds: Vec<u64> = {
        let mut rounds: Vec<u64> = dropped.iter().map(|entry| entry.round).collect();
        rounds.sort_unstable();
        rounds.dedup();
        rounds
    };

    for block in &mut analyses {
        block.dropped = dropped_rounds.contains(&block.round);
        if let Some(entry) = dropped.iter().find(|entry| {
            entry.round == block.round && entry.label == block.label && entry.kind == block.kind
        }) {
            block.drop_reason = Some(entry.reason);
        }
    }

    let mut interval_seconds: Vec<f64> = samples.iter().map(|sample| sample.seconds).collect();
    let workloads = summarize(&pairs, &dropped_rounds);

    Ok(Analysis {
        sample_count: samples.len(),
        median_interval_seconds: median(&mut interval_seconds),
        idle_median_mw: median(&mut idle_powers.clone()),
        idle_min_mw: minimum(&idle_powers),
        idle_max_mw: maximum(&idle_powers),
        blocks: analyses,
        workloads,
        dropped_blocks: dropped,
        dropped_rounds,
    })
}

fn base_analysis(block: &BlockRow) -> BlockAnalysis {
    BlockAnalysis {
        round: block.round,
        label: block.label.clone(),
        kind: block.kind,
        start: block.start,
        end: block.end,
        count: block.count,
        dropped: false,
        average_mw: None,
        skipped: None,
        idle_mw: None,
        work_seconds: None,
        work_mj: None,
        net_mj: None,
        per_run_mj: None,
        drop_reason: None,
    }
}

/// Pairs each work block with the idle block immediately before it and computes its energy.
fn pair_blocks(
    samples: &[PowerSample],
    blocks: &[BlockRow],
    analyses: &mut [BlockAnalysis],
) -> Result<Vec<WorkPair>, String> {
    let mut pairs = Vec::new();

    for (index, block) in blocks.iter().enumerate() {
        if let Some(average) = mean_power_mw(samples, block.start, block.end) {
            analyses[index].average_mw = Some(average);
        }
        if block.kind != BlockKind::Work {
            continue;
        }

        let idle = match index.checked_sub(1).map(|before| &blocks[before]) {
            Some(idle) if idle.kind == BlockKind::Idle => idle,
            _ => {
                analyses[index].skipped = Some("no idle block before the work block".to_owned());
                continue;
            }
        };
        let Some(idle_mw) = mean_power_mw(samples, idle.start, idle.end) else {
            analyses[index].skipped = Some("no samples in the idle window".to_owned());
            continue;
        };
        let work_samples = samples_inside(samples, block.start, block.end);
        if work_samples.is_empty() {
            analyses[index].skipped = Some("no samples in the work window".to_owned());
            continue;
        }
        if block.count == 0 {
            return Err(format!(
                "Block {} {} ran no commands",
                block.round, block.label
            ));
        }

        let mut work_seconds = 0.0;
        let mut work_mj = 0.0;
        for sample in &work_samples {
            work_seconds += sample.seconds;
            work_mj += sample.power_mw * sample.seconds;
        }
        let work_average_mw = work_mj / work_seconds;
        // Scale to the block's full length, since partial samples at its edges are excluded.
        let duration_seconds = block.end - block.start;
        let net_mj = (work_mj - idle_mw * work_seconds) * (duration_seconds / work_seconds);

        pairs.push(WorkPair {
            block_index: index,
            block: block.clone(),
            idle_mw,
            work_average_mw,
            work_seconds,
            work_mj,
            net_mj,
            per_run_mj: net_mj / block.count as f64,
        });
    }

    Ok(pairs)
}

/// The median work power of each workload, in the order the workloads first appear.
fn median_work_power(pairs: &[WorkPair]) -> Vec<(String, f64)> {
    let mut by_label: Vec<(String, Vec<f64>)> = Vec::new();
    for pair in pairs {
        push_to_group(&mut by_label, &pair.block.label, pair.work_average_mw);
    }
    by_label
        .into_iter()
        .map(|(label, mut values)| (label, median(&mut values)))
        .collect()
}

/// Drops a whole round from its first disturbed block, in the round's own block order.
///
/// The comparison uses every round's work power for the workload, candidate included.
fn find_dropped_blocks(
    blocks: &[BlockRow],
    analyses: &[BlockAnalysis],
    workload_medians: &[(String, f64)],
) -> Vec<DroppedBlock> {
    let mut dropped = Vec::new();
    let mut rounds: Vec<u64> = blocks.iter().map(|block| block.round).collect();
    rounds.sort_unstable();
    rounds.dedup();

    for round in rounds {
        for (index, block) in blocks.iter().enumerate() {
            if block.round != round {
                continue;
            }
            let Some(average) = analyses[index].average_mw else {
                continue;
            };
            if block.kind == BlockKind::Idle && average > MAXIMUM_IDLE_MW {
                dropped.push(DroppedBlock {
                    round,
                    label: block.label.clone(),
                    kind: block.kind,
                    reason: DropReason::Idle,
                    observed_mw: average,
                    limit_mw: MAXIMUM_IDLE_MW,
                });
                break;
            }
            if block.kind == BlockKind::Work {
                let Some((_, workload_median)) = workload_medians
                    .iter()
                    .find(|(label, _)| *label == block.label)
                else {
                    continue;
                };
                let limit = workload_median * WORK_SPIKE_FACTOR;
                if average > limit {
                    dropped.push(DroppedBlock {
                        round,
                        label: block.label.clone(),
                        kind: block.kind,
                        reason: DropReason::Work,
                        observed_mw: average,
                        limit_mw: limit,
                    });
                    break;
                }
            }
        }
    }

    dropped
}

/// The per-workload medians over all rounds and over the clean ones.
fn summarize(pairs: &[WorkPair], dropped_rounds: &[u64]) -> Vec<WorkloadAnalysis> {
    let mut by_label: Vec<(String, Vec<&WorkPair>)> = Vec::new();
    for pair in pairs {
        push_to_group(&mut by_label, &pair.block.label, pair);
    }

    by_label
        .into_iter()
        .map(|(label, values)| {
            let all: Vec<f64> = values.iter().map(|pair| pair.per_run_mj).collect();
            let clean: Vec<f64> = values
                .iter()
                .filter(|pair| !dropped_rounds.contains(&pair.block.round))
                .map(|pair| pair.per_run_mj)
                .collect();
            let mut counts: Vec<f64> = values.iter().map(|pair| pair.block.count as f64).collect();
            let all_stats = describe(&all);

            WorkloadAnalysis {
                label,
                runs_per_block: median(&mut counts).round() as u64,
                median_mj: all_stats.median,
                min_mj: all_stats.min,
                max_mj: all_stats.max,
                median_micro_watt_hours: all_stats.median / MILLIJOULES_PER_MICRO_WATT_HOUR,
                spread: all_stats.spread,
                verdict: if all_stats.spread > NOISY_SPREAD {
                    Verdict::Noisy
                } else {
                    Verdict::Ok
                },
                clean_median_mj: some_describe(&clean).map(|stats| stats.median),
                clean_min_mj: if clean.is_empty() {
                    None
                } else {
                    Some(minimum(&clean))
                },
                clean_max_mj: if clean.is_empty() {
                    None
                } else {
                    Some(maximum(&clean))
                },
                clean_spread: some_describe(&clean).map(|stats| stats.spread),
                clean_rounds: clean.len(),
                clean_verdict: if clean.len() < MINIMUM_CLEAN_ROUNDS {
                    CleanVerdict::Rerun
                } else {
                    CleanVerdict::Ok
                },
            }
        })
        .collect()
}

struct Describe {
    median: f64,
    min: f64,
    max: f64,
    spread: f64,
}

fn describe(values: &[f64]) -> Describe {
    let middle = median(&mut values.to_vec());
    let min = minimum(values);
    let max = maximum(values);
    Describe {
        median: middle,
        min,
        max,
        spread: if middle > 0.0 {
            (max - min) / middle
        } else {
            f64::INFINITY
        },
    }
}

fn some_describe(values: &[f64]) -> Option<Describe> {
    if values.is_empty() {
        None
    } else {
        Some(describe(values))
    }
}

fn push_to_group<T>(groups: &mut Vec<(String, Vec<T>)>, label: &str, value: T) {
    match groups.iter_mut().find(|(name, _)| name == label) {
        Some((_, values)) => values.push(value),
        None => groups.push((label.to_owned(), vec![value])),
    }
}

/// The middle value, or the mean of the middle two when the count is even.
fn median(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty(), "median needs at least one value");
    values.sort_by(|left, right| left.total_cmp(right));
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn minimum(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

fn maximum(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// The table printed after a run, one line per row.
pub fn format_analysis(analysis: &Analysis) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "samples: {}, median interval {:.3} s",
        analysis.sample_count, analysis.median_interval_seconds
    ));
    lines.push(format!(
        "idle power: median {:.0} mW, range {:.0} to {:.0} mW",
        analysis.idle_median_mw, analysis.idle_min_mw, analysis.idle_max_mw
    ));
    lines.push(String::new());
    lines.push(format_dropped(&analysis.dropped_blocks));
    lines.push(String::new());
    lines.push(
        [
            "command",
            "runs/block",
            "median mJ",
            "min mJ",
            "max mJ",
            "median uWh",
            "spread",
            "verdict",
            "clean median mJ",
            "clean rounds",
            "clean verdict",
        ]
        .join("\t"),
    );
    for workload in &analysis.workloads {
        lines.push(
            [
                workload.label.clone(),
                workload.runs_per_block.to_string(),
                format!("{:.1}", workload.median_mj),
                format!("{:.1}", workload.min_mj),
                format!("{:.1}", workload.max_mj),
                format!("{:.1}", workload.median_micro_watt_hours),
                format_spread(workload.spread),
                format_verdict(workload.verdict),
                format_optional(workload.clean_median_mj),
                workload.clean_rounds.to_string(),
                format_clean_verdict(workload.clean_verdict),
            ]
            .join("\t"),
        );
    }
    lines
}

fn format_dropped(dropped: &[DroppedBlock]) -> String {
    if dropped.is_empty() {
        return "dropped blocks: none".to_owned();
    }
    let mut lines = vec![format!("dropped blocks: {}", dropped.len())];
    for entry in dropped {
        let limit = format!("{:.0} mW", entry.limit_mw);
        lines.push(format!(
            "  round {} {} ({}): {:.0} mW, limit {}",
            entry.round,
            entry.label,
            entry.kind.as_str(),
            entry.observed_mw,
            limit
        ));
    }
    lines.join("\n")
}

fn format_spread(spread: f64) -> String {
    if !spread.is_finite() {
        return "n/a".to_owned();
    }
    format!("{:.0}%", spread * PERCENT)
}

fn format_verdict(verdict: Verdict) -> String {
    match verdict {
        Verdict::Ok => "ok",
        Verdict::Noisy => "noisy",
    }
    .to_owned()
}

fn format_clean_verdict(verdict: CleanVerdict) -> String {
    match verdict {
        CleanVerdict::Ok => "ok",
        CleanVerdict::Rerun => "rerun",
    }
    .to_owned()
}

fn format_optional(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.DECIMAL_PLACES$}"),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_START: f64 = 1_000.0;

    fn sample(start_seconds: f64, power_mw: f64) -> PowerSample {
        PowerSample {
            start_seconds,
            end_seconds: start_seconds + 1.0,
            power_mw,
            seconds: 1.0,
        }
    }

    fn seconds_of(start_seconds: f64, count: usize, power_mw: f64) -> Vec<PowerSample> {
        (0..count)
            .map(|index| sample(start_seconds + index as f64, power_mw))
            .collect()
    }

    fn idle(round: u64, label: &str, start_seconds: f64, end_seconds: f64) -> BlockRow {
        BlockRow {
            round,
            label: label.to_owned(),
            kind: BlockKind::Idle,
            start: start_seconds,
            end: end_seconds,
            count: 0,
        }
    }

    fn work(round: u64, label: &str, start_seconds: f64, end_seconds: f64, count: u64) -> BlockRow {
        BlockRow {
            round,
            label: label.to_owned(),
            kind: BlockKind::Work,
            start: start_seconds,
            end: end_seconds,
            count,
        }
    }

    /// Ten idle seconds, then ten work seconds, both one sample per second.
    fn pair(
        round: u64,
        label: &str,
        start_seconds: f64,
        idle_power_mw: f64,
        work_power_mw: f64,
        count: u64,
    ) -> (Vec<PowerSample>, Vec<BlockRow>) {
        (
            [
                seconds_of(start_seconds, 10, idle_power_mw),
                seconds_of(start_seconds + 10.0, 10, work_power_mw),
            ]
            .concat(),
            [
                idle(round, label, start_seconds, start_seconds + 10.0),
                work(
                    round,
                    label,
                    start_seconds + 10.0,
                    start_seconds + 20.0,
                    count,
                ),
            ]
            .to_vec(),
        )
    }

    fn analyze_rounds(rounds: &[(u64, f64, f64)]) -> Analysis {
        let mut samples = Vec::new();
        let mut blocks = Vec::new();
        for (round, idle_power, work_power) in rounds {
            let (round_samples, round_blocks) = pair(
                *round,
                "x",
                SEED_START + *round as f64 * 100.0,
                *idle_power,
                *work_power,
                10,
            );
            samples.extend(round_samples);
            blocks.extend(round_blocks);
        }
        analyze(&samples, &blocks).expect("the run analyzes")
    }

    #[test]
    fn energy_per_command_subtracts_idle_power_and_divides_by_the_run_count() {
        let (samples, blocks) = pair(1, "x", 0.0, 500.0, 1_500.0, 100);
        let analysis = analyze(&samples, &blocks).expect("the run analyzes");
        assert_eq!(analysis.idle_median_mw, 500.0);
        assert_eq!(analysis.workloads.len(), 1);

        let workload = &analysis.workloads[0];
        assert!(
            (workload.median_mj - 100.0).abs() < 1e-9,
            "{}",
            workload.median_mj
        );
        assert!((workload.median_micro_watt_hours - 100.0 / 3.6).abs() < 1e-9);
        assert_eq!(workload.verdict, Verdict::Ok);
        assert_eq!(workload.clean_verdict, CleanVerdict::Rerun);
        assert_eq!(workload.clean_rounds, 1);
        assert_eq!(workload.runs_per_block, 100);
    }

    #[test]
    fn an_idle_spike_drops_its_whole_round() {
        let rounds: Vec<(u64, f64, f64)> = (1..=5)
            .map(|round| {
                let idle = if round == 4 {
                    MAXIMUM_IDLE_MW + 500.0
                } else {
                    400.0
                };
                (round, idle, 1_000.0)
            })
            .collect();
        let analysis = analyze_rounds(&rounds);
        assert_eq!(analysis.dropped_rounds, [4]);
        assert_eq!(analysis.dropped_blocks.len(), 1);
        assert_eq!(analysis.dropped_blocks[0].reason, DropReason::Idle);
        assert_eq!(analysis.workloads[0].clean_rounds, MINIMUM_CLEAN_ROUNDS);
        assert_eq!(analysis.workloads[0].clean_verdict, CleanVerdict::Ok);
        assert!(
            analysis
                .blocks
                .iter()
                .filter(|block| block.round == 4)
                .all(|block| block.dropped)
        );
        assert_eq!(
            analysis
                .blocks
                .iter()
                .filter(|block| block.drop_reason.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn a_work_spike_above_one_and_a_half_times_the_median_drops_its_round() {
        let rounds: Vec<(u64, f64, f64)> = (1..=5)
            .map(|round| {
                let work = if round == 2 { 2_000.0 } else { 1_000.0 };
                (round, 400.0, work)
            })
            .collect();
        let analysis = analyze_rounds(&rounds);
        assert_eq!(analysis.dropped_rounds, [2]);
        assert_eq!(analysis.dropped_blocks[0].reason, DropReason::Work);
        assert_eq!(analysis.workloads[0].clean_rounds, 4);
        let limit = analysis.dropped_blocks[0].limit_mw;
        assert!((limit - 1_500.0).abs() < 1e-9, "{limit}");
    }

    #[test]
    fn a_clean_run_drops_nothing() {
        let rounds: Vec<(u64, f64, f64)> = (1..=5).map(|round| (round, 400.0, 1_000.0)).collect();
        let analysis = analyze_rounds(&rounds);
        assert_eq!(analysis.dropped_rounds, Vec::<u64>::new());
        assert!(analysis.dropped_blocks.is_empty());
        assert_eq!(analysis.workloads[0].clean_rounds, 5);
        let workload = &analysis.workloads[0];
        let clean = workload.clean_median_mj.expect("clean median");
        assert!((clean - workload.median_mj).abs() < 1e-9);
    }

    #[test]
    fn fewer_than_four_clean_rounds_marks_the_workload_for_a_rerun() {
        let rounds: Vec<(u64, f64, f64)> = (1..=5)
            .map(|round| {
                let idle = if round == 2 || round == 5 {
                    2_000.0
                } else {
                    400.0
                };
                (round, idle, 1_000.0)
            })
            .collect();
        let analysis = analyze_rounds(&rounds);
        assert_eq!(analysis.dropped_rounds, [2, 5]);
        assert_eq!(analysis.workloads[0].clean_rounds, 3);
        assert_eq!(analysis.workloads[0].clean_verdict, CleanVerdict::Rerun);
    }

    #[test]
    fn a_work_block_without_an_idle_before_it_is_skipped() {
        let samples = seconds_of(0.0, 10, 1_000.0);
        let blocks = [work(1, "x", 0.0, 10.0, 10)];
        let error = analyze(&samples, &blocks).expect_err("nothing pairs");
        assert_eq!(error, "no work block has samples: nothing to analyze");
    }

    #[test]
    fn a_work_block_counts_no_commands() {
        let (samples, mut blocks) = pair(1, "x", 0.0, 500.0, 1_500.0, 0);
        blocks[1].count = 0;
        let error = analyze(&samples, &blocks).expect_err("no run count");
        assert_eq!(error, "Block 1 x ran no commands");
    }

    #[test]
    fn no_samples_is_refused() {
        let error = analyze(&[], &[work(1, "x", 0.0, 10.0, 10)]).expect_err("no samples");
        assert!(error.starts_with("no usable samples"), "{error}");
    }

    #[test]
    fn the_median_averages_the_middle_two_of_an_even_count() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn the_fixture_logs_give_energy_per_run() {
        let samples =
            crate::samples::parse_power_log(include_str!("../tests/fixtures/power.plist"));
        let blocks = crate::blocks::parse_blocks(include_str!("../tests/fixtures/blocks.tsv"))
            .expect("the fixture block log parses");
        let analysis = analyze(&samples, &blocks).expect("the fixture analyzes");

        // Idle is 115 mW for four seconds, work is 155 mW for four seconds, so the
        // work block carries 620 mJ, 460 mJ of which the idle block explains.
        assert_eq!(analysis.sample_count, 12);
        assert_eq!(analysis.median_interval_seconds, 1.0);
        assert_eq!(analysis.idle_median_mw, 115.0);

        let block = &analysis.blocks[1];
        assert_eq!(block.average_mw, Some(155.0));
        assert_eq!(block.idle_mw, Some(115.0));
        assert_eq!(block.work_seconds, Some(4.0));
        assert!((block.work_mj.expect("work energy") - 620.0).abs() < 1e-9);
        assert!((block.net_mj.expect("net energy") - 160.0).abs() < 1e-9);

        let workload = &analysis.workloads[0];
        assert_eq!(workload.runs_per_block, 120);
        assert!((workload.median_mj - 160.0 / 120.0).abs() < 1e-12);
        assert_eq!(workload.clean_rounds, 1);
        assert_eq!(workload.clean_verdict, CleanVerdict::Rerun);
        assert!(analysis.dropped_blocks.is_empty());
    }
}
