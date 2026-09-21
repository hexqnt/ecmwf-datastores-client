use log::Level;
use url::Url;

/// Converts a textual severity into a `log::Level`.
pub fn level_from_severity(severity: &str) -> Level {
    if ["CRITICAL", "FATAL", "ERROR"]
        .iter()
        .any(|level| severity.eq_ignore_ascii_case(level))
    {
        Level::Error
    } else if ["WARNING", "WARN"]
        .iter()
        .any(|level| severity.eq_ignore_ascii_case(level))
    {
        Level::Warn
    } else if severity.eq_ignore_ascii_case("DEBUG") {
        Level::Debug
    } else {
        Level::Info
    }
}

/// Splits a message prefixed by a severity into `(Level, message)`.
pub fn split_prefixed_level(message: &str) -> (Level, &str) {
    const LEVELS: &[(&str, Level)] = &[
        ("CRITICAL", Level::Error),
        ("FATAL", Level::Error),
        ("ERROR", Level::Error),
        ("WARNING", Level::Warn),
        ("WARN", Level::Warn),
        ("INFO", Level::Info),
        ("DEBUG", Level::Debug),
        ("NOTSET", Level::Trace),
    ];
    for (prefix, level) in LEVELS {
        if let Some(rest) = message.strip_prefix(prefix) {
            if !rest.is_empty() && !rest.starts_with([':', ' ', '\t']) {
                continue;
            }
            let content = rest.trim_start().trim_start_matches(':').trim();
            return (*level, if content.is_empty() { message } else { content });
        }
    }
    (Level::Info, message)
}

/// Appends a trailing slash to the URL path when missing.
pub fn ensure_trailing_slash(url: &mut Url) {
    if !url.path().ends_with('/') {
        let mut path = url.path().to_string();
        path.push('/');
        url.set_path(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_severity_and_rejects_partial_prefixes() {
        assert_eq!(level_from_severity("error"), Level::Error);
        assert_eq!(level_from_severity("WaRn"), Level::Warn);
        assert_eq!(
            split_prefixed_level("ERROR: failed"),
            (Level::Error, "failed")
        );
        assert_eq!(
            split_prefixed_level("ERROR : failed"),
            (Level::Error, "failed")
        );
        assert_eq!(
            split_prefixed_level("ERRORS found"),
            (Level::Info, "ERRORS found")
        );
    }
}
