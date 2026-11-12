//! The order workloads run in, round by round.

use crate::random::Random;

/// A seeded Fisher-Yates order for each round, stable for a given seed and label list.
///
/// One generator feeds every round, so the orders differ between rounds and
/// repeat exactly when the same seed and labels are passed again.
pub fn round_orders<L>(labels: &[L], rounds: usize, seed: u32) -> Vec<Vec<String>>
where
    L: AsRef<str>,
{
    let mut random = Random::new(seed);
    let mut orders = Vec::with_capacity(rounds);

    for _ in 0..rounds {
        let mut order: Vec<String> = labels
            .iter()
            .map(|label| label.as_ref().to_owned())
            .collect();
        for index in (1..order.len()).rev() {
            let swap = random.integer(0, index as u64) as usize;
            order.swap(index, swap);
        }
        orders.push(order);
    }

    orders
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: u32 = 20_261_106;

    #[test]
    fn six_labels_keep_their_reference_orders() {
        let labels = ["a", "b", "c", "d", "e", "f"];
        let orders = round_orders(&labels, 5, SEED);
        assert_eq!(
            orders,
            [
                ["d", "b", "f", "c", "e", "a"],
                ["e", "d", "f", "a", "c", "b"],
                ["c", "b", "f", "d", "e", "a"],
                ["f", "b", "e", "a", "c", "d"],
                ["d", "a", "f", "e", "c", "b"],
            ]
        );
    }

    #[test]
    fn a_recorded_run_keeps_its_block_order() {
        let labels = [
            "node",
            "list@100",
            "list-json@100",
            "view@100",
            "create@100",
            "list@10000",
        ];
        let orders = round_orders(&labels, 5, SEED);
        assert_eq!(
            orders,
            [
                [
                    "view@100",
                    "list@100",
                    "list@10000",
                    "list-json@100",
                    "create@100",
                    "node",
                ],
                [
                    "create@100",
                    "view@100",
                    "list@10000",
                    "node",
                    "list-json@100",
                    "list@100",
                ],
                [
                    "list-json@100",
                    "list@100",
                    "list@10000",
                    "view@100",
                    "create@100",
                    "node",
                ],
                [
                    "list@10000",
                    "list@100",
                    "create@100",
                    "node",
                    "list-json@100",
                    "view@100",
                ],
                [
                    "view@100",
                    "node",
                    "list@10000",
                    "create@100",
                    "list-json@100",
                    "list@100",
                ],
            ]
        );
    }

    #[test]
    fn every_round_visits_every_label_once() {
        let labels = ["a", "b", "c", "d", "e", "f", "g"];
        for order in round_orders(&labels, 20, 42) {
            let mut sorted = order.clone();
            sorted.sort();
            assert_eq!(sorted, ["a", "b", "c", "d", "e", "f", "g"]);
        }
    }

    #[test]
    fn the_same_seed_repeats_the_orders() {
        let labels = ["a", "b", "c", "d", "e"];
        assert_eq!(
            round_orders(&labels, 5, SEED),
            round_orders(&labels, 5, SEED)
        );
    }

    #[test]
    fn another_seed_reorders_the_rounds() {
        let labels = ["a", "b", "c", "d", "e"];
        assert_ne!(
            round_orders(&labels, 5, SEED),
            round_orders(&labels, 5, SEED + 1)
        );
    }

    #[test]
    fn no_rounds_means_no_orders() {
        assert!(round_orders(&["a", "b"], 0, SEED).is_empty());
    }

    #[test]
    fn one_label_keeps_every_round_the_same() {
        let orders = round_orders(&["only"], 3, SEED);
        assert_eq!(orders, [["only"], ["only"], ["only"]]);
    }
}
