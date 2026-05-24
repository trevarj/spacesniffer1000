use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub name_substring: String,
    pub min_size_bytes: u128,
    pub max_age_days: u64,
}

#[derive(Debug, Clone)]
pub struct FilterEntry<'a> {
    pub name: &'a str,
    pub size: u128,
    pub modified: SystemTime,
}

impl Filters {
    pub fn matches(&self, entry: &FilterEntry<'_>, now: SystemTime) -> bool {
        let needle = self.name_substring.trim().to_lowercase();
        if !needle.is_empty() && !entry.name.to_lowercase().contains(&needle) {
            return false;
        }

        if entry.size < self.min_size_bytes {
            return false;
        }

        if self.max_age_days > 0 {
            let max_age = Duration::from_secs(self.max_age_days.saturating_mul(86_400));
            match now.duration_since(entry.modified) {
                Ok(age) if age > max_age => return false,
                Err(_) => return false,
                _ => {}
            }
        }

        true
    }
}

pub fn parse_size_input_bytes(input: &str) -> Option<u128> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(0);
    }

    let split_at = trimmed
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    if number.is_empty() {
        return None;
    }

    let value = number.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1_f64,
        "k" | "kb" | "kib" => 1024_f64,
        "m" | "mb" | "mib" => 1024_f64.powi(2),
        "g" | "gb" | "gib" => 1024_f64.powi(3),
        "t" | "tb" | "tib" => 1024_f64.powi(4),
        _ => return None,
    };

    Some((value * multiplier).round().clamp(0.0, u128::MAX as f64) as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_name_size_and_age_filters() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 86_400);
        let recent = now - Duration::from_secs(86_400);
        let old = now - Duration::from_secs(8 * 86_400);
        let filters = Filters {
            name_substring: "cache".into(),
            min_size_bytes: 100,
            max_age_days: 7,
        };

        assert!(filters.matches(
            &FilterEntry {
                name: "build-cache",
                size: 150,
                modified: recent,
            },
            now,
        ));
        assert!(!filters.matches(
            &FilterEntry {
                name: "target",
                size: 150,
                modified: recent,
            },
            now,
        ));
        assert!(!filters.matches(
            &FilterEntry {
                name: "build-cache",
                size: 1,
                modified: recent,
            },
            now,
        ));
        assert!(!filters.matches(
            &FilterEntry {
                name: "build-cache",
                size: 150,
                modified: old,
            },
            now,
        ));
    }

    #[test]
    fn parses_size_input_suffixes_as_binary_units() {
        assert_eq!(parse_size_input_bytes("512"), Some(512));
        assert_eq!(parse_size_input_bytes("1K"), Some(1024));
        assert_eq!(parse_size_input_bytes("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_input_bytes("3G"), Some(3 * 1024 * 1024 * 1024));
        assert_eq!(parse_size_input_bytes("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_size_input_bytes("2 tib"), Some(2 * 1024_u128.pow(4)));
    }

    #[test]
    fn rejects_invalid_size_input() {
        assert_eq!(parse_size_input_bytes(""), Some(0));
        assert_eq!(parse_size_input_bytes("abc"), None);
        assert_eq!(parse_size_input_bytes("12XB"), None);
        assert_eq!(parse_size_input_bytes("-4G"), None);
        assert_eq!(parse_size_input_bytes("1.2.3G"), None);
    }
}
