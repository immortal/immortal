//! Compact duration and byte-size quantity codec shared by the `log` schema.
//!
//! `log.age` and `log.size` accept either a plain integer (seconds or
//! mebibytes) or a compact suffixed quantity such as `1h` or `2MiB`.
//! [`deserialize_optional_log_age`] and [`deserialize_optional_log_size`]
//! decode both spellings into stored seconds and bytes for
//! [`super::wire::FileLogInput`], while [`format_log_age`] and
//! [`format_log_size`] re-encode stored values back into that same compact
//! spelling for [`super::super::model::FileLogConfig`]'s serializer. Every
//! parsed quantity is treated as a positive compact-suffix integer: unknown
//! or missing suffixes, non-digit or leading-zero digit runs, a zero
//! quantity, and multiplication overflow are all rejected here rather than
//! silently truncated or accepted downstream.

use serde::{Deserialize, Deserializer, de::Error as _};

use super::super::MEBIBYTE;

#[derive(Deserialize)]
#[serde(untagged)]
enum LogScalar {
    Integer(u64),
    Text(String),
}

pub(super) fn deserialize_optional_log_age<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<LogScalar>::deserialize(deserializer)?
        .map(parse_log_age)
        .transpose()
        .map_err(D::Error::custom)
}

pub(super) fn deserialize_optional_log_size<'de, D>(
    deserializer: D,
) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<LogScalar>::deserialize(deserializer)?
        .map(parse_log_size)
        .transpose()
        .map_err(D::Error::custom)
}

fn parse_log_age(value: LogScalar) -> Result<u64, String> {
    match value {
        LogScalar::Integer(seconds) => positive_value(seconds, "log age"),
        LogScalar::Text(duration) => parse_compact_quantity(
            &duration,
            &[
                ("w", 7 * 24 * 60 * 60),
                ("d", 24 * 60 * 60),
                ("h", 60 * 60),
                ("m", 60),
                ("s", 1),
            ],
            "log age",
        ),
    }
}

fn parse_log_size(value: LogScalar) -> Result<u64, String> {
    match value {
        LogScalar::Integer(mebibytes) => positive_value(mebibytes, "log size")?
            .checked_mul(MEBIBYTE)
            .ok_or_else(|| "log size exceeds the supported byte range".to_owned()),
        LogScalar::Text(size) => parse_compact_quantity(
            &size,
            &[
                ("GiB", 1024 * MEBIBYTE),
                ("MiB", MEBIBYTE),
                ("KiB", 1024),
                ("B", 1),
            ],
            "log size",
        ),
    }
}

fn parse_compact_quantity(
    value: &str,
    units: &[(&str, u64)],
    description: &str,
) -> Result<u64, String> {
    let Some((digits, multiplier)) = units.iter().find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(*suffix)
            .map(|digits| (digits, *multiplier))
    }) else {
        return Err(format!("{description} has an unknown or missing unit"));
    };
    if digits.is_empty()
        || !digits.as_bytes().iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits.as_bytes().first() == Some(&b'0'))
    {
        return Err(format!("{description} must use a positive compact integer"));
    }
    let quantity = digits
        .parse::<u64>()
        .map_err(|_| format!("{description} exceeds the supported numeric range"))?;
    positive_value(quantity, description)?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{description} exceeds the supported numeric range"))
}

fn positive_value(value: u64, description: &str) -> Result<u64, String> {
    if value == 0 {
        Err(format!("{description} must be greater than zero"))
    } else {
        Ok(value)
    }
}

/// Render a rotation age using the same compact unit accepted by `log.age`.
pub(in super::super) fn format_log_age(seconds: u64) -> String {
    format_quantity(
        seconds,
        &[
            ("w", 7 * 24 * 60 * 60),
            ("d", 24 * 60 * 60),
            ("h", 60 * 60),
            ("m", 60),
            ("s", 1),
        ],
    )
}

/// Render a rotation threshold using the same compact unit accepted by `log.size`.
pub(in super::super) fn format_log_size(bytes: u64) -> String {
    format_quantity(
        bytes,
        &[
            ("GiB", 1024 * MEBIBYTE),
            ("MiB", MEBIBYTE),
            ("KiB", 1024),
            ("B", 1),
        ],
    )
}

fn format_quantity(value: u64, units: &[(&str, u64)]) -> String {
    units
        .iter()
        .find(|(_, multiplier)| value >= *multiplier && value.is_multiple_of(*multiplier))
        .map_or_else(
            || value.to_string(),
            |(suffix, multiplier)| format!("{}{suffix}", value / multiplier),
        )
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::config::{FileLogRoutes, emit_config, parse_str};

    #[test]
    fn logging_duration_and_size_units_are_checked_and_canonical() -> Result<(), Box<dyn Error>> {
        for (value, expected) in [
            ("1s", 1),
            ("2m", 120),
            ("3h", 10_800),
            ("4d", 345_600),
            ("2w", 1_209_600),
        ] {
            let source = format!(
                "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: {value}\n"
            );
            let config = parse_str(&source)?;
            let Some(FileLogRoutes::Combined(file)) = config.logging.files else {
                return Err("combined age route is missing".into());
            };
            assert_eq!(file.max_age_seconds, Some(expected));
            assert_eq!(file.keep, Some(7));
        }

        for (value, expected) in [
            ("1B", 1),
            ("2KiB", 2_048),
            ("3MiB", 3_145_728),
            ("4GiB", 4_294_967_296),
            ("2", 2_097_152),
        ] {
            let source = format!(
                "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: {value}\n"
            );
            let config = parse_str(&source)?;
            let Some(FileLogRoutes::Combined(file)) = config.logging.files else {
                return Err("combined size route is missing".into());
            };
            assert_eq!(file.max_bytes, Some(expected));
            assert_eq!(file.keep, Some(7));
        }

        let canonical = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 3600\n  size: 1024KiB\n",
        )?;
        let emitted = emit_config(&canonical)?;
        assert!(emitted.contains("age: 1h"));
        assert!(emitted.contains("size: 1MiB"));
        Ok(())
    }
}
