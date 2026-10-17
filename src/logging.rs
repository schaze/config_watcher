use std::{borrow::Cow, error::Error, fmt};

/// Byte limit for document text in log fields below TRACE.
pub(crate) const PAYLOAD_MAX: usize = 512;

/// Cuts `value` to at most `max_bytes` at a UTF-8 boundary and appends `…(+N bytes)`.
pub(crate) fn truncate(value: &str, max_bytes: usize) -> Cow<'_, str> {
    if value.len() <= max_bytes {
        return Cow::Borrowed(value);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…(+{} bytes)", &value[..end], value.len() - end))
}

/// Formats an error and its sources on one line: `outer: cause: root cause`.
pub(crate) struct ErrorChain<'a>(&'a (dyn Error + 'static));

pub(crate) fn chain<'a>(err: &'a (dyn Error + 'static)) -> ErrorChain<'a> {
    ErrorChain(err)
}

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(err) = source {
            write!(f, ": {err}")?;
            source = err.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WatcherError;
    use std::{io, path::PathBuf};

    #[test]
    fn truncate_keeps_short_values() {
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_cuts_at_char_boundary() {
        // 'ä' is two bytes; a cut at byte 2 would split it
        assert_eq!(truncate("aäb", 2), "a…(+3 bytes)");
        assert_eq!(truncate("aäb", 3), "aä…(+1 bytes)");
    }

    #[test]
    fn chain_joins_sources_on_one_line() {
        let err = WatcherError::FileReadError(
            PathBuf::from("/config/a.yaml"),
            io::Error::new(io::ErrorKind::NotFound, "not found"),
        );
        assert_eq!(
            chain(&err).to_string(),
            "cannot read file /config/a.yaml: not found"
        );
    }
}
