//! Duration strings used throughout moxy.xml, such as `500ms`, `60s`, `5m`,
//! `24h`, `30d` or compounds like `1h30m`. A bare integer means seconds.

use std::time::Duration;

pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("duration is empty".into());
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return Err(format!("invalid duration '{input}'"));
        }
        let n: u64 = rest[..digits]
            .parse()
            .map_err(|_| format!("invalid duration '{input}'"))?;
        rest = &rest[digits..];
        let unit_len = rest.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let part = match unit {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n * 60),
            "h" => Duration::from_secs(n * 3600),
            "d" => Duration::from_secs(n * 86_400),
            _ => {
                return Err(format!(
                    "invalid duration unit in '{input}' (use ms, s, m, h or d)"
                ));
            }
        };
        total += part;
    }
    Ok(total)
}

/// Render a duration in the compact form accepted by [`parse_duration`].
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let s = ms / 1000;
    if s > 0 && s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s > 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s > 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(
            parse_duration("30d").unwrap(),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("5 minutes").is_err());
        assert!(parse_duration("5y").is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
    }
}
