//! UTC calendar time for session ids, log lines and sample timestamps.
//!
//! Session timestamps are written as `YYYY-MM-DDTHH:MM:SSZ`, so a samples file
//! stays readable in a text editor and still parses back to the Unix seconds the
//! block log uses. Only UTC is handled; a local time would need the time zone
//! database and would make two runs on either side of a clock change ambiguous.

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_MINUTE: i64 = 60;

/// Formats Unix milliseconds as `YYYY-MM-DDTHH:MM:SSZ`, dropping milliseconds.
pub fn iso8601_seconds(milliseconds: u64) -> String {
    let total = (milliseconds / 1_000) as i64;
    let days = total.div_euclid(SECONDS_PER_DAY);
    let seconds = total.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / SECONDS_PER_HOUR;
    let minute = seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let second = seconds % SECONDS_PER_MINUTE;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Reads `YYYY-MM-DDTHH:MM:SSZ` back to Unix seconds.
///
/// An optional fractional part is accepted and dropped, since the writer never
/// emits one. Anything else, including a local offset, is refused.
pub fn parse_iso8601_seconds(text: &str) -> Option<f64> {
    let body = text.trim().strip_suffix('Z')?;
    let (date, time) = body.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || year < 1970 || !(1..=12).contains(&month) {
        return None;
    }

    let mut time_parts = time.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let seconds_text = time_parts.next()?;
    if time_parts.next().is_some() {
        return None;
    }
    let seconds_text = seconds_text
        .split_once('.')
        .map_or(seconds_text, |(whole, _)| whole);
    let second: u32 = seconds_text.parse().ok()?;
    if hour >= 24 || minute >= 60 || second >= 60 || !(1..=31).contains(&day) {
        return None;
    }

    let days = days_from_civil(year, month, day);
    // Reject a day the month does not have, such as 2026-02-30.
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let total = days * SECONDS_PER_DAY
        + i64::from(hour) * SECONDS_PER_HOUR
        + i64::from(minute) * SECONDS_PER_MINUTE
        + i64::from(second);
    Some(total as f64)
}

/// The date part of a timestamp, `YYYY-MM-DD`.
fn date_of(milliseconds: u64) -> (i64, u32, u32) {
    civil_from_days((milliseconds / 1_000) as i64 / SECONDS_PER_DAY)
}

/// `HH:MM:SS` in UTC, for a log line.
pub fn clock(milliseconds: u64) -> String {
    let text = iso8601_seconds(milliseconds);
    text[11..19].to_owned()
}

/// `YYYY-MM-DD HH:MM` in UTC, for the plan's expected end.
pub fn date_time(milliseconds: u64) -> String {
    let (year, month, day) = date_of(milliseconds);
    let text = iso8601_seconds(milliseconds);
    format!("{year:04}-{month:02}-{day:02} {}", &text[11..16])
}

/// `YYYYMMDD-HHMMSS` in UTC, the name of one session folder.
pub fn session_id(milliseconds: u64) -> String {
    let (year, month, day) = date_of(milliseconds);
    let text = iso8601_seconds(milliseconds);
    let time: String = text[11..19]
        .chars()
        .filter(|character| *character != ':')
        .collect();
    format!("{year:04}{month:02}{day:02}-{time}")
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture power log starts here; its comment names the same instant.
    const BASE_SECONDS: u64 = 1_794_132_000;

    #[test]
    fn the_epoch_is_written_the_way_the_reader_expects() {
        assert_eq!(iso8601_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_seconds(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(
            iso8601_seconds(BASE_SECONDS * 1_000 + 5_000),
            "2026-11-08T10:00:05Z"
        );
    }

    #[test]
    fn a_timestamp_reads_back_to_its_seconds() {
        for milliseconds in [0, 1_000, BASE_SECONDS * 1_000, 1_800_000_000_000] {
            let text = iso8601_seconds(milliseconds);
            assert_eq!(
                parse_iso8601_seconds(&text),
                Some((milliseconds / 1_000) as f64),
                "{text}"
            );
        }
    }

    #[test]
    fn a_fractional_part_is_accepted_and_dropped() {
        assert_eq!(
            parse_iso8601_seconds("2026-11-08T10:00:05.250Z"),
            Some(BASE_SECONDS as f64 + 5.0)
        );
    }

    #[test]
    fn a_timestamp_outside_the_format_is_refused() {
        for text in [
            "",
            "2026-11-08",
            "2026-11-08T10:00:05",
            "2026-11-08 10:00:05Z",
            "2026-11-08T10:00:05+01:00",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-11-08T24:00:00Z",
            "x026-11-08T10:00:05Z",
        ] {
            assert_eq!(parse_iso8601_seconds(text), None, "{text}");
        }
    }

    #[test]
    fn clock_date_and_session_id_come_from_the_same_instant() {
        let milliseconds = (BASE_SECONDS + 3_723) * 1_000;
        assert_eq!(clock(milliseconds), "11:02:03");
        assert_eq!(date_time(milliseconds), "2026-11-08 11:02");
        assert_eq!(session_id(milliseconds), "20261108-110203");
    }
}
