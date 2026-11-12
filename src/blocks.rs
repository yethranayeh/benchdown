//! The `blocks.tsv` reader and writer.
//!
//! The runner that writes this file lands in a later commit; until then these
//! functions are reached by the tests only.

#![allow(dead_code)]

/// The columns of a block log, in order, as its header spells them.
pub const BLOCK_COLUMNS: [&str; 6] = ["round", "label", "kind", "start", "end", "count"];

/// Which half of a round a block belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Idle,
    Work,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Work => "work",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "idle" => Some(Self::Idle),
            "work" => Some(Self::Work),
            _ => None,
        }
    }
}

/// One block as recorded in a block log.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockRow {
    pub round: u64,
    pub label: String,
    pub kind: BlockKind,
    /// Unix seconds with milliseconds.
    pub start: f64,
    /// Unix seconds with milliseconds.
    pub end: f64,
    /// How many times the command ran inside the block.
    pub count: u64,
}

pub fn format_header() -> String {
    BLOCK_COLUMNS.join("\t")
}

pub fn format_row(row: &BlockRow) -> String {
    format!(
        "{}\t{}\t{}\t{:.3}\t{:.3}\t{}",
        row.round,
        row.label,
        row.kind.as_str(),
        row.start,
        row.end,
        row.count
    )
}

/// Reads a block log, refusing anything the runner could not have written.
pub fn parse_blocks(text: &str) -> Result<Vec<BlockRow>, String> {
    let mut lines = text.lines().filter(|line| !line.is_empty());
    let Some(header) = lines.next() else {
        return Err("The block log is empty".to_owned());
    };
    let expected = format_header();
    if header != expected {
        return Err(format!("The block log header must be {expected}"));
    }

    let mut rows = Vec::new();
    for (offset, line) in lines.enumerate() {
        let number = offset + 2;
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.len() != BLOCK_COLUMNS.len() {
            return Err(format!(
                "Block log line {number} has {} columns",
                cells.len()
            ));
        }

        let round: u64 = cells[0]
            .parse()
            .map_err(|_| format!("Block log line {number} has no round number"))?;
        if round < 1 {
            return Err(format!("Block log line {number} has no round number"));
        }
        let kind = BlockKind::parse(cells[2])
            .ok_or_else(|| format!("Block log line {number} has kind {}", cells[2]))?;
        let start = cells[3].parse::<f64>().unwrap_or(f64::NAN);
        let end = cells[4].parse::<f64>().unwrap_or(f64::NAN);
        if !start.is_finite() || !end.is_finite() || end < start {
            return Err(format!("Block log line {number} has no valid window"));
        }
        let count: u64 = cells[5]
            .parse()
            .map_err(|_| format!("Block log line {number} has no valid run count"))?;

        rows.push(BlockRow {
            round,
            label: cells[1].to_owned(),
            kind,
            start,
            end,
            count,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_LOG: &str = include_str!("../tests/fixtures/blocks.tsv");

    fn row(round: u64, kind: BlockKind, start: f64, end: f64, count: u64) -> BlockRow {
        BlockRow {
            round,
            label: "one".to_owned(),
            kind,
            start,
            end,
            count,
        }
    }

    #[test]
    fn the_fixture_log_parses() {
        let rows = parse_blocks(BLOCK_LOG).expect("the fixture parses");
        assert_eq!(
            rows,
            [
                row(1, BlockKind::Idle, 1_794_132_000.0, 1_794_132_004.0, 0),
                row(1, BlockKind::Work, 1_794_132_004.0, 1_794_132_008.0, 120),
            ]
        );
    }

    #[test]
    fn a_written_log_reads_back_the_same() {
        let rows = [
            row(1, BlockKind::Idle, 1_794_132_000.0, 1_794_132_004.5, 0),
            row(2, BlockKind::Work, 1_794_132_004.5, 1_794_132_008.25, 7),
        ];
        let mut text = format_header();
        text.push('\n');
        for entry in &rows {
            text.push_str(&format_row(entry));
            text.push('\n');
        }
        assert_eq!(parse_blocks(&text), Ok(rows.to_vec()));
        assert!(text.contains("1794132004.500\t1794132008.250"));
    }

    #[test]
    fn crlf_lines_are_accepted() {
        let text = BLOCK_LOG.replace('\n', "\r\n");
        assert_eq!(parse_blocks(&text), parse_blocks(BLOCK_LOG));
    }

    #[test]
    fn an_empty_log_is_refused() {
        assert_eq!(parse_blocks(""), Err("The block log is empty".to_owned()));
    }

    #[test]
    fn a_wrong_header_is_refused() {
        for header in [
            "round\tlabel\tkind\tstart\tend",
            "label\tround\tkind\tstart\tend\tcount",
            "round\tlabel\tkind\tstart\tend\tcount\textra",
        ] {
            let error = parse_blocks(&format!("{header}\n")).expect_err("wrong header");
            assert!(error.contains("header must be"), "{header}: {error}");
        }
    }

    #[test]
    fn a_missing_column_is_refused() {
        let error = parse_blocks("round\tlabel\tkind\tstart\tend\tcount\n1\tone\tidle\t1.0\t2.0\n")
            .expect_err("five columns");
        assert!(error.contains("line 2 has 5 columns"), "{error}");
    }

    #[test]
    fn a_row_without_a_round_number_is_refused() {
        for round in ["", "x", "0", "-1", "1.5"] {
            let text = format!("{}\n{round}\tone\tidle\t1.0\t2.0\t0\n", format_header());
            let error = parse_blocks(&text).expect_err("no round number");
            assert!(error.contains("has no round number"), "{round}: {error}");
        }
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let text = format!("{}\n1\tone\tpause\t1.0\t2.0\t0\n", format_header());
        let error = parse_blocks(&text).expect_err("unknown kind");
        assert!(error.contains("has kind pause"), "{error}");
    }

    #[test]
    fn a_backwards_or_missing_window_is_refused() {
        for window in [("3.0", "2.0"), ("x", "2.0"), ("1.0", "inf")] {
            let text = format!(
                "{}\n1\tone\tidle\t{}\t{}\t0\n",
                format_header(),
                window.0,
                window.1
            );
            let error = parse_blocks(&text).expect_err("no valid window");
            assert!(error.contains("has no valid window"), "{error}");
        }
    }

    #[test]
    fn a_bad_run_count_is_refused() {
        let text = format!("{}\n1\tone\tidle\t1.0\t2.0\t-1\n", format_header());
        let error = parse_blocks(&text).expect_err("negative count");
        assert!(error.contains("has no valid run count"), "{error}");
    }
}
