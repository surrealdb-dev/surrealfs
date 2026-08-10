//! Parsing the timestamp shapes this project deals in.
//!
//! Two callers need this: the mount layer, which derives a file's mtime from the commit that
//! last wrote it, and the CLI, which resolves a time reference to a commit. It lives here so
//! there is one parser rather than two that drift — the mount's copy came first, and moving it
//! is the whole reason this module exists.
//!
//! Hand-rolled rather than pulling in a date library. The only absolute input is our own
//! `type::string(datetime)` output plus what a user types on a command line, both of which are a
//! single well-defined shape.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parse an RFC 3339 timestamp in the shape SurrealDB emits: `2026-08-07T11:45:26.909868Z`.
///
/// Fractional seconds are optional and are padded or truncated to nanoseconds. A trailing `Z` is
/// accepted and required in practice; offsets other than UTC are not supported, because nothing
/// in this system produces one.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_once('T')?;
    let time = rest.trim_end_matches('Z');
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let seconds_part = time_parts.next()?;
    let (secs_text, nanos) = match seconds_part.split_once('.') {
        Some((s, frac)) => {
            let padded = format!("{frac:0<9}");
            (s, padded[..9].parse::<u32>().ok()?)
        }
        None => (seconds_part, 0),
    };
    let second: i64 = secs_text.parse().ok()?;

    // Days since the Unix epoch, by the standard civil-from-days algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let total = days * 86_400 + hour * 3_600 + minute * 60 + second;
    if total < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(total as u64, nanos))
}

/// Format a `SystemTime` back into the shape [`parse_rfc3339`] accepts.
///
/// Used to hand a time to the database as a comparable string, so the value that goes out is in
/// the same form as the values already stored.
/// Sub-second precision is preserved. Dropping it would silently shift a formatted time
/// *backwards* by up to a second, which is enough for a query at a commit's own instant to miss
/// that commit — the stored timestamps carry microseconds.
pub fn format_rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let total = since.as_secs() as i64;
    let nanos = since.subsec_nanos();
    let days = total.div_euclid(86_400);
    let secs_of_day = total.rem_euclid(86_400);

    // days-from-civil, inverted.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // Whole seconds print without a fraction, so the common case stays readable and round-trips
    // to exactly the text it came from.
    if nanos == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}.{nanos:09}Z")
    }
}

/// Parse a relative duration written the way people say it: `90s`, `30m`, `2h`, `7d`.
///
/// Deliberately small. A general duration grammar invites `1h30m` and then time zones and then a
/// date library; a single unit covers what a time reference on a command line actually needs.
pub fn parse_relative(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (value, unit) = text.split_at(text.len().checked_sub(1)?);
    let value: u64 = value.parse().ok()?;
    let seconds = match unit {
        "s" => value,
        "m" => value.checked_mul(60)?,
        "h" => value.checked_mul(3_600)?,
        "d" => value.checked_mul(86_400)?,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_from_the_database_parse() {
        let parsed = parse_rfc3339("2026-08-07T11:45:26.909868Z").expect("parses");
        let secs = parsed.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_786_103_126);
    }

    #[test]
    fn a_timestamp_without_a_fraction_parses() {
        assert!(parse_rfc3339("2026-01-01T00:00:00Z").is_some());
        assert_eq!(
            parse_rfc3339("1970-01-01T00:00:00Z").unwrap(),
            UNIX_EPOCH,
            "the epoch itself must parse"
        );
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed() {
        assert!(parse_rfc3339("not a timestamp").is_none());
        assert!(parse_rfc3339("2026-08-07").is_none());
        assert!(parse_rfc3339("").is_none());
    }

    /// Round-tripping matters because the CLI formats a time to hand to the database, and the
    /// database's own output is what comes back.
    #[test]
    fn formatting_and_parsing_are_inverses() {
        for text in [
            "1970-01-01T00:00:00Z",
            "2000-02-29T12:34:56Z",
            "2026-08-07T11:45:26Z",
            "2100-12-31T23:59:59Z",
        ] {
            let parsed = parse_rfc3339(text).expect(text);
            assert_eq!(format_rfc3339(parsed), text, "round trip failed for {text}");
        }
    }

    /// Truncating to whole seconds shifts a time backwards by up to a second, which is enough
    /// for a query at a commit's own instant to miss that commit. Found by a test that did
    /// exactly that.
    #[test]
    fn sub_second_precision_survives_formatting() {
        let text = "2026-08-10T12:44:04.328554Z";
        let parsed = parse_rfc3339(text).unwrap();
        let formatted = format_rfc3339(parsed);
        assert!(
            formatted.starts_with("2026-08-10T12:44:04.328554"),
            "microseconds were dropped: {formatted}"
        );
        assert_eq!(
            parse_rfc3339(&formatted).unwrap(),
            parsed,
            "the formatted value must parse back to the same instant"
        );
    }

    #[test]
    fn relative_durations_cover_the_units_people_use() {
        assert_eq!(parse_relative("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_relative("30m"), Some(Duration::from_secs(1_800)));
        assert_eq!(parse_relative("2h"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_relative("7d"), Some(Duration::from_secs(604_800)));
    }

    #[test]
    fn an_unsupported_duration_is_rejected_rather_than_misread() {
        // No compound forms, no bare numbers, no unknown units: each would otherwise resolve to
        // something plausible and wrong.
        for bad in ["1h30m", "5", "5y", "h", "", "-1h", "1 h"] {
            assert!(parse_relative(bad).is_none(), "accepted {bad:?}");
        }
    }
}
