//! `--health-*` options: the one family of container `options:` Greenlit
//! interprets rather than rejects.
//!
//! `PHASE-4-environment.md` ("Service containers"): "honor `--health-*`
//! options; poll until healthy or timeout". Before Phase 4 every unrecognized
//! flag fell through to `UnsupportedOption`, so a perfectly ordinary
//! `options: --health-cmd redis-cli ping` was refused at run time even though
//! it planned cleanly.
//!
//! Only the five health flags are interpreted. Everything else keeps its
//! existing treatment, including the containment-breaking rejections — a
//! service is as untrusted as a job container, and `--privileged` on a
//! service would be exactly as dangerous.
//!
//! # Duration spelling
//!
//! Docker accepts Go duration strings (`10s`, `1m30s`, `500ms`), which is what
//! workflow authors write. They are parsed here, once, into the nanoseconds
//! the API models durations in, rather than passed through as text for a
//! backend to re-parse.

use crate::engine::HealthCheck;

/// A `--health-*` flag that could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthOptionError {
    /// The flag as authored.
    pub flag: String,
    /// The value that could not be read.
    pub value: String,
}

/// Consumes a `--health-*` flag, returning whether it was one.
///
/// `value` is the flag's argument, whether it arrived as `--flag=value` or as
/// the following token.
///
/// # Errors
/// Returns [`HealthOptionError`] when the flag is a health flag whose value
/// cannot be read — an unparseable duration or retry count is a workflow
/// mistake worth naming, not something to silently ignore.
pub(crate) fn apply(
    health: &mut HealthCheck,
    flag: &str,
    value: Option<&str>,
) -> Result<bool, HealthOptionError> {
    let missing = || HealthOptionError {
        flag: flag.to_string(),
        value: String::new(),
    };
    let bad = |value: &str| HealthOptionError {
        flag: flag.to_string(),
        value: value.to_string(),
    };

    match flag {
        "--health-cmd" => {
            let value = value.ok_or_else(missing)?;
            // Docker's `CMD-SHELL` form runs the string through a shell,
            // which is what `--health-cmd` means on the command line.
            health.test = vec!["CMD-SHELL".to_string(), value.to_string()];
        }
        "--health-interval" => {
            let value = value.ok_or_else(missing)?;
            health.interval_nanos = Some(duration_nanos(value).ok_or_else(|| bad(value))?);
        }
        "--health-timeout" => {
            let value = value.ok_or_else(missing)?;
            health.timeout_nanos = Some(duration_nanos(value).ok_or_else(|| bad(value))?);
        }
        "--health-start-period" => {
            let value = value.ok_or_else(missing)?;
            health.start_period_nanos = Some(duration_nanos(value).ok_or_else(|| bad(value))?);
        }
        "--health-retries" => {
            let value = value.ok_or_else(missing)?;
            health.retries = Some(value.trim().parse().map_err(|_| bad(value))?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Whether a health flag takes a following token as its value.
///
/// Every one of them does; this exists so the option scanner can skip that
/// token rather than reading it as the next flag.
pub(crate) fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--health-cmd"
            | "--health-interval"
            | "--health-timeout"
            | "--health-start-period"
            | "--health-retries"
    )
}

/// Parses a Go-style duration (`10s`, `1m30s`, `500ms`, `1h`) to nanoseconds.
///
/// A bare number is seconds, matching how Docker reads a unitless value.
fn duration_nanos(text: &str) -> Option<i64> {
    const NANOS_PER: [(&str, i64); 5] = [
        ("ns", 1),
        ("us", 1_000),
        ("ms", 1_000_000),
        ("s", 1_000_000_000),
        ("m", 60 * 1_000_000_000),
    ];

    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(seconds) = text.parse::<i64>() {
        return seconds.checked_mul(1_000_000_000);
    }

    let mut total: i64 = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
        let (number, tail) = rest.split_at(digits);
        let amount: i64 = number.parse().ok()?;

        // Longest unit first so `ms` is not read as `m` followed by `s`.
        let (unit, nanos) = ["ns", "us", "ms", "h", "s", "m"]
            .into_iter()
            .find(|unit| tail.starts_with(unit))
            .and_then(|unit| {
                if unit == "h" {
                    Some((unit, 3600 * 1_000_000_000_i64))
                } else {
                    NANOS_PER
                        .iter()
                        .find(|(name, _)| *name == unit)
                        .map(|(name, nanos)| (*name, *nanos))
                }
            })?;

        total = total.checked_add(amount.checked_mul(nanos)?)?;
        rest = &tail[unit.len()..];
    }
    Some(total)
}
