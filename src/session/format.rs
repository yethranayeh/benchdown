//! Durations, estimates and shell quoting for the plan, the log and status lines.

/// A span of milliseconds as `45s`, `7m 30s` or `1h 2m`.
pub fn duration(milliseconds: u64) -> String {
    let total_seconds = milliseconds.saturating_add(500) / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// A job estimate in minutes as `30s`, `3 min` or `1h 5m`.
pub fn estimate(minutes: f64) -> String {
    if minutes < 1.0 {
        return format!("{}s", (minutes * 60.0).round() as u64);
    }
    if minutes < 60.0 {
        return format!("{} min", minutes.round() as u64);
    }
    let whole = minutes.round() as u64;
    format!("{}h {}m", whole / 60, whole % 60)
}

/// Quotes one shell word for display and for a state file's manual undo line.
pub fn shell_quote(word: &str) -> String {
    let safe = word
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_@%+=:,./-".contains(character));
    if safe && !word.is_empty() {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// One command line with every word quoted where it needs to be.
pub fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_is_shown_by_its_largest_unit() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(90_000), "1m 30s");
        assert_eq!(duration(3_723_000), "1h 2m");
    }

    #[test]
    fn an_estimate_uses_seconds_minutes_or_hours() {
        assert_eq!(estimate(0.5), "30s");
        assert_eq!(estimate(3.0), "3 min");
        assert_eq!(estimate(65.0), "1h 5m");
    }

    #[test]
    fn a_safe_word_is_not_quoted() {
        assert_eq!(shell_quote("/System/Volumes/Data"), "/System/Volumes/Data");
        assert_eq!(shell_quote("en0"), "en0");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_word_with_spaces_or_quotes_is_quoted() {
        assert_eq!(shell_quote("/Volumes/My Disk"), "'/Volumes/My Disk'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_command_line_quotes_only_what_needs_it() {
        assert_eq!(
            command_line(&[
                "sudo".to_owned(),
                "mdutil".to_owned(),
                "-i".to_owned(),
                "on".to_owned(),
                "/Volumes/My Disk".to_owned()
            ]),
            "sudo mdutil -i on '/Volumes/My Disk'"
        );
    }
}
