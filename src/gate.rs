//! Waiting for a quiet machine before each block.
//!
//! The check that finds blocks disturbed by background work needs the session
//! samples and lands with the quiet-session port; the types it fills live here.

use serde::Serialize;

use crate::blocks::BlockKind;

/// A one-minute load average above this means the machine is busy.
/// Two clean quiet sessions on an M4 MacBook Pro stayed below 3.1, a normal desktop with apps open sat near 4.4.
pub const QUIET_LOAD: f64 = 3.5;

/// The longest wait for a quiet machine before one block.
pub const QUIET_WAIT_LIMIT_SECONDS: u64 = 60;

const QUIET_POLL_SECONDS: u64 = 5;
const MILLISECONDS_PER_SECOND: u64 = 1_000;

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
///
/// The check that fills these needs the session samples, so no variant is
/// constructed until the quiet-session port lands.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
