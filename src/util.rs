//! Small shared helpers.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Parse a human duration such as `30s`, `25m`, `2h`, or `90` (bare seconds).
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let (digits, unit) = match s.chars().last().expect("non-empty") {
        c if c.is_ascii_digit() => (s, 's'),
        c => (&s[..s.len() - c.len_utf8()], c.to_ascii_lowercase()),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("`{raw}` is not a duration like 30s, 25m or 2h"))?;
    let secs = match unit {
        's' => value,
        'm' => value * 60,
        'h' => value * 3600,
        'd' => value * 86_400,
        other => bail!("unknown duration unit `{other}` in `{raw}` (use s, m, h or d)"),
    };
    Ok(Duration::from_secs(secs))
}

/// Render a duration the way a human would say it: `2h 05m`, `7m 12s`, `45s`.
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A filesystem-safe version of an arbitrary string.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("25m").unwrap(), Duration::from_secs(1500));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration(" 1d ").unwrap(), Duration::from_secs(86_400));
    }

    #[test]
    fn rejects_nonsense_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("10y").is_err());
    }

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(432)), "7m 12s");
        assert_eq!(format_duration(Duration::from_secs(7500)), "2h 05m");
    }

    #[test]
    fn slugifies() {
        assert_eq!(slugify("Fix the CI/CD pipeline!"), "fix-the-ci-cd-pipeline");
        assert_eq!(slugify("   "), "");
    }
}
