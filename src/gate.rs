//! Waiting for a quiet machine before each block, and finding blocks that
//! background work disturbed.

use serde::Serialize;

use crate::blocks::{BlockKind, BlockRow};
use crate::session::sampler::SessionSample;

/// A one-minute load average above this means the machine is busy.
/// Two clean quiet sessions on an M4 MacBook Pro stayed below 3.1, a normal desktop with apps open sat near 4.4.
pub const QUIET_LOAD: f64 = 3.5;

/// The longest wait for a quiet machine before one block.
pub const QUIET_WAIT_LIMIT_SECONDS: u64 = 60;

const QUIET_POLL_SECONDS: u64 = 5;
const MILLISECONDS_PER_SECOND: u64 = 1_000;

/// A named background process at or above this CPU share disturbs the block it falls in.
pub const NOISY_PROCESS_CPU_PERCENT: f64 = 20.0;

/// Background services known to wake up on an idle Mac: indexing, backups, photo
/// and Siri analysis, updates. A process name matches a prefix.
const NOISY_PROCESSES: [&str; 9] = [
    "mds",
    "mds_stores",
    "mdworker",
    "backupd",
    "photoanalysisd",
    "softwareupdated",
    "siriknowledged",
    "contactsd",
    "suggestd",
];

/// One wait for a quiet machine before a block.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuietWait {
    pub round: u64,
    pub label: String,
    pub load_before: f64,
    pub load_after: f64,
    pub waited_seconds: u64,
    /// True when the load was still high at the limit and the block ran anyway.
    pub timed_out: bool,
}

/// Why a block was marked as disturbed by background work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContaminationReason {
    Load,
    Process,
}

/// A block the session samples marked as disturbed by background work.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContaminatedBlock {
    pub round: u64,
    pub label: String,
    pub kind: BlockKind,
    pub reason: ContaminationReason,
    /// The load average, or the process's CPU percent.
    pub observed: f64,
    pub limit: f64,
    /// The process name, for a process reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Waits while the one-minute load is above the quiet level, and gives up at the limit.
pub fn wait_for_quiet(
    round: u64,
    label: &str,
    read_load: &mut dyn FnMut() -> Result<f64, String>,
    sleep: &dyn Fn(u64),
) -> Result<Option<QuietWait>, String> {
    let load_before = read_load()?;
    if load_before <= QUIET_LOAD {
        return Ok(None);
    }

    let mut load = load_before;
    let mut waited_seconds = 0;
    while load > QUIET_LOAD && waited_seconds < QUIET_WAIT_LIMIT_SECONDS {
        sleep(QUIET_POLL_SECONDS * MILLISECONDS_PER_SECOND);
        waited_seconds += QUIET_POLL_SECONDS;
        load = read_load()?;
    }

    Ok(Some(QuietWait {
        round,
        label: label.to_owned(),
        load_before,
        load_after: load,
        waited_seconds,
        timed_out: load > QUIET_LOAD,
    }))
}

/// One entry per disturbed block, with the first reason found in the block's samples.
///
/// The window includes both ends, as the session's samples sit on whole seconds.
pub fn find_contaminated_blocks(
    samples: &[SessionSample],
    blocks: &[BlockRow],
) -> Vec<ContaminatedBlock> {
    let mut contaminated = Vec::new();
    for block in blocks {
        let inside = samples
            .iter()
            .filter(|sample| sample.seconds >= block.start && sample.seconds <= block.end);
        if let Some((reason, observed, limit, process)) = first_reason(inside) {
            contaminated.push(ContaminatedBlock {
                round: block.round,
                label: block.label.clone(),
                kind: block.kind,
                reason,
                observed,
                limit,
                process,
            });
        }
    }
    contaminated
}

/// The first reason in one block's samples: high load first, then a busy named process.
fn first_reason<'a>(
    samples: impl Iterator<Item = &'a SessionSample>,
) -> Option<(ContaminationReason, f64, f64, Option<String>)> {
    for sample in samples {
        if sample.load1 > QUIET_LOAD {
            return Some((ContaminationReason::Load, sample.load1, QUIET_LOAD, None));
        }
        for entry in &sample.processes {
            let name = entry.command.rsplit('/').next().unwrap_or(&entry.command);
            let noisy = NOISY_PROCESSES
                .iter()
                .any(|prefix| name.starts_with(prefix));
            if noisy && entry.cpu_percent >= NOISY_PROCESS_CPU_PERCENT {
                return Some((
                    ContaminationReason::Process,
                    entry.cpu_percent,
                    NOISY_PROCESS_CPU_PERCENT,
                    Some(name.to_owned()),
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::sampler::SessionProcess;
    use std::cell::RefCell;

    fn scripted(values: Vec<f64>) -> impl FnMut() -> Result<f64, String> {
        let values = RefCell::new(values.into_iter());
        move || {
            Ok(values
                .borrow_mut()
                .next()
                .expect("the script has enough values"))
        }
    }

    fn sample(seconds: f64, load1: f64, processes: &[(&str, f64)]) -> SessionSample {
        SessionSample {
            seconds,
            load1,
            processes: processes
                .iter()
                .map(|(command, cpu_percent)| SessionProcess {
                    cpu_percent: *cpu_percent,
                    command: (*command).to_owned(),
                })
                .collect(),
        }
    }

    fn block(round: u64, kind: BlockKind, start: f64, end: f64) -> BlockRow {
        BlockRow {
            round,
            label: "node".to_owned(),
            kind,
            start,
            end,
            count: if kind == BlockKind::Work { 10 } else { 0 },
        }
    }

    #[test]
    fn a_quiet_machine_starts_the_block_without_waiting() {
        let mut read = scripted(vec![QUIET_LOAD - 1.0]);
        let slept = RefCell::new(Vec::new());
        let sleep = |milliseconds: u64| slept.borrow_mut().push(milliseconds);
        let wait = wait_for_quiet(1, "node", &mut read, &sleep).expect("the load reads");
        assert_eq!(wait, None);
        assert!(slept.borrow().is_empty());
    }

    #[test]
    fn a_busy_machine_waits_until_the_load_falls() {
        let mut read = scripted(vec![QUIET_LOAD + 2.0, QUIET_LOAD + 1.0, QUIET_LOAD - 1.0]);
        let slept = RefCell::new(Vec::new());
        let sleep = |milliseconds: u64| slept.borrow_mut().push(milliseconds);
        let wait = wait_for_quiet(2, "node", &mut read, &sleep)
            .expect("the load reads")
            .expect("it waited");

        assert_eq!(wait.round, 2);
        assert_eq!(wait.label, "node");
        assert!(!wait.timed_out);
        assert_eq!(wait.load_after, QUIET_LOAD - 1.0);
        assert_eq!(wait.waited_seconds, 10);
        assert_eq!(*slept.borrow(), [5_000, 5_000]);
    }

    #[test]
    fn the_wait_stops_at_its_limit_and_the_block_runs_anyway() {
        let mut read = scripted(vec![QUIET_LOAD + 2.0; 20]);
        let slept = RefCell::new(Vec::new());
        let sleep = |milliseconds: u64| slept.borrow_mut().push(milliseconds);
        let wait = wait_for_quiet(1, "node", &mut read, &sleep)
            .expect("the load reads")
            .expect("it waited");

        assert!(wait.timed_out);
        assert_eq!(wait.waited_seconds, QUIET_WAIT_LIMIT_SECONDS);
        assert_eq!(
            slept.borrow().len(),
            (QUIET_WAIT_LIMIT_SECONDS / QUIET_POLL_SECONDS) as usize
        );
    }

    #[test]
    fn blocks_with_high_load_or_a_busy_noisy_process_are_contaminated() {
        let blocks = [
            block(1, BlockKind::Idle, 0.0, 20.0),
            block(1, BlockKind::Work, 20.0, 40.0),
            block(2, BlockKind::Idle, 40.0, 60.0),
        ];
        let samples = [
            sample(5.0, 1.5, &[("/usr/libexec/PerfPowerServices", 90.0)]),
            sample(
                25.0,
                1.5,
                &[(
                    "/System/Library/Frameworks/CoreServices.framework/mds_stores",
                    NOISY_PROCESS_CPU_PERCENT,
                )],
            ),
            sample(45.0, QUIET_LOAD + 0.5, &[]),
            sample(70.0, QUIET_LOAD + 5.0, &[]),
        ];

        assert_eq!(
            find_contaminated_blocks(&samples, &blocks),
            [
                ContaminatedBlock {
                    round: 1,
                    label: "node".to_owned(),
                    kind: BlockKind::Work,
                    reason: ContaminationReason::Process,
                    observed: NOISY_PROCESS_CPU_PERCENT,
                    limit: NOISY_PROCESS_CPU_PERCENT,
                    process: Some("mds_stores".to_owned()),
                },
                ContaminatedBlock {
                    round: 2,
                    label: "node".to_owned(),
                    kind: BlockKind::Idle,
                    reason: ContaminationReason::Load,
                    observed: QUIET_LOAD + 0.5,
                    limit: QUIET_LOAD,
                    process: None,
                },
            ]
        );
    }

    #[test]
    fn the_first_reason_in_a_block_wins() {
        let blocks = [block(1, BlockKind::Work, 10.0, 20.0)];
        let samples = [
            sample(11.0, 4.0, &[("mds", 90.0)]),
            sample(12.0, 1.0, &[("backupd", 30.0)]),
        ];
        let contaminated = find_contaminated_blocks(&samples, &blocks);
        assert_eq!(contaminated.len(), 1);
        assert_eq!(contaminated[0].reason, ContaminationReason::Load);
        assert_eq!(contaminated[0].observed, 4.0);
        assert_eq!(contaminated[0].process, None);
    }

    #[test]
    fn a_block_window_includes_both_ends() {
        let blocks = [block(1, BlockKind::Work, 10.0, 20.0)];
        let samples = [
            sample(9.999, 9.0, &[]),
            sample(10.0, 4.0, &[]),
            sample(20.0, 4.0, &[]),
            sample(20.001, 9.0, &[]),
        ];
        let contaminated = find_contaminated_blocks(&samples, &blocks);
        assert_eq!(contaminated.len(), 1);
        assert_eq!(
            contaminated[0].observed, 4.0,
            "the first sample inside wins"
        );
    }

    #[test]
    fn a_quiet_block_with_unrelated_processes_is_clean() {
        let blocks = [block(1, BlockKind::Work, 0.0, 20.0)];
        let samples = [
            sample(5.0, QUIET_LOAD, &[("mds_stores", 19.9)]),
            sample(10.0, 1.0, &[("Xcode", 95.0), ("mdworker_shared", 5.0)]),
        ];
        assert!(find_contaminated_blocks(&samples, &blocks).is_empty());
    }

    #[test]
    fn a_process_name_matches_by_prefix() {
        let blocks = [block(1, BlockKind::Work, 0.0, 20.0)];
        let samples = [sample(5.0, 1.0, &[("/usr/libexec/mdworker_shared", 25.0)])];
        let contaminated = find_contaminated_blocks(&samples, &blocks);
        assert_eq!(contaminated.len(), 1);
        assert_eq!(contaminated[0].process.as_deref(), Some("mdworker_shared"));
    }
}
