//! Seeded random numbers for the block schedule.
//!
//! Ported bit for bit from Zinn's `createRandom`, so one seed gives the same
//! order in both harnesses.

/// mulberry32: a small, fast 32-bit generator.
#[derive(Debug, Clone)]
pub struct Random {
    state: u32,
}

impl Random {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// A float in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut value = self.state;
        value = (value ^ (value >> 15)).wrapping_mul(value | 1);
        value ^= value.wrapping_add((value ^ (value >> 7)).wrapping_mul(value | 61));
        f64::from(value ^ (value >> 14)) / 4_294_967_296.0
    }

    /// An integer in `[min, max]`, both included.
    pub fn integer(&mut self, min: u64, max: u64) -> u64 {
        assert!(min <= max, "an integer range cannot run backwards");
        min + (self.next_f64() * (max - min + 1) as f64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Printed by Zinn's `createRandom(20261106)` for its first ten `next()` calls.
    const FIRST_TEN: [f64; 10] = [
        0.01353957294486463,
        0.8884504192974418,
        0.7106646823231131,
        0.18516792636364698,
        0.8750997642055154,
        0.3228561542928219,
        0.4405779654625803,
        0.08609710983000696,
        0.5739861070178449,
        0.3789441881235689,
    ];

    #[test]
    fn floats_match_the_reference_generator() {
        let mut random = Random::new(20_261_106);
        for expected in FIRST_TEN {
            assert_eq!(random.next_f64(), expected);
        }
    }

    #[test]
    fn floats_stay_inside_the_unit_interval() {
        let mut random = Random::new(1);
        for _ in 0..10_000 {
            let value = random.next_f64();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn integers_continue_the_same_stream() {
        let mut random = Random::new(20_261_106);
        for _ in 0..FIRST_TEN.len() {
            random.next_f64();
        }
        let drawn: Vec<u64> = (0..10).map(|_| random.integer(0, 5)).collect();
        assert_eq!(drawn, [0, 5, 4, 1, 4, 3, 2, 1, 4, 3]);
    }

    #[test]
    fn integers_can_touch_both_ends() {
        let mut random = Random::new(7);
        let drawn: Vec<u64> = (0..1_000).map(|_| random.integer(3, 5)).collect();
        assert!(drawn.contains(&3));
        assert!(drawn.contains(&5));
        assert!(drawn.iter().all(|value| (3..=5).contains(value)));
    }
}
