//! Escaped, tagged values for text written to diagnostic logs.

use std::fmt::{self, Display, Write};
use std::panic::Location;
use tracing::Subscriber;
use tracing_subscriber::filter::{filter_fn, FilterExt, LevelFilter};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{self as tracing_fmt, format::Writer, MakeWriter};
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

pub(crate) const RECORD_TAG: &str = "[lwm-log-v1] ";
const SPAN_TAG: &str = "@lwm:";

pub struct RecordTime;

impl FormatTime for RecordTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        writer.write_str(RECORD_TAG)?;
        SystemTime.format_time(writer)
    }
}

pub fn file_layer<S, W>(writer: W, level: LevelFilter) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    tracing_fmt::layer()
        .with_ansi(false)
        .with_timer(RecordTime)
        .with_writer(writer)
        .with_filter(level.and(filter_fn(|metadata| {
            // Third-party tracing and log events have not been audited for spans.
            matches!(
                metadata.target().split("::").next(),
                Some("leopardwm" | "leopardwm_platform_win32" | "leopardwm_watchdog")
            )
        })))
}

pub fn panic_record(location: Option<&Location<'_>>, payload: &str) -> String {
    if let Some(location) = location {
        // Keep line/column even if a custom build embeds a private source path.
        error_record(format_args!(
            "panic at {}:{}:{}: {}",
            path(location.file()),
            location.line(),
            location.column(),
            text(payload)
        ))
    } else {
        error_record(format_args!("panic: {}", text(payload)))
    }
}

pub fn error_record(message: fmt::Arguments<'_>) -> String {
    let mut record = String::new();
    RecordTime
        .format_time(&mut Writer::new(&mut record))
        .unwrap();
    writeln!(record, " ERROR {message}").unwrap();
    record
}

pub fn panic_payload(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

pub(crate) fn single_line(value: &str) -> impl Display + '_ {
    struct Value<'a>(&'a str);
    impl Display for Value<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Escaped(f, false).write_str(self.0)
        }
    }
    Value(value)
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Title,
    Path,
    Text,
    Name,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Path => "path",
            Self::Text => "text",
            Self::Name => "name",
        }
    }
}

pub(crate) fn split_span(input: &str) -> Option<(Kind, &str, String, &str)> {
    let (tag, quoted) = input.split_once('"')?;
    let kind = [Kind::Title, Kind::Path, Kind::Text, Kind::Name]
        .into_iter()
        .find(|kind| kind.tag() == tag)?;
    let mut escaped = false;
    for (index, ch) in quoted.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            let encoded = &quoted[..index];
            return Some((kind, encoded, unescape(encoded)?, &quoted[index + 1..]));
        }
    }
    None
}

pub(crate) fn before_span(input: &str) -> Option<(&str, &str)> {
    input.split_once(SPAN_TAG)
}

pub struct LogText<T> {
    kind: Kind,
    value: T,
}

pub fn title<T: Display>(value: T) -> LogText<T> {
    LogText {
        kind: Kind::Title,
        value,
    }
}

pub fn path<T: Display>(value: T) -> LogText<T> {
    LogText {
        kind: Kind::Path,
        value,
    }
}

pub fn text<T: Display>(value: T) -> LogText<T> {
    LogText {
        kind: Kind::Text,
        value,
    }
}

pub fn name<T: Display>(value: T) -> LogText<T> {
    LogText {
        kind: Kind::Name,
        value,
    }
}

struct Escaped<'a, 'b>(&'a mut fmt::Formatter<'b>, bool);

impl Write for Escaped<'_, '_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for ch in value.chars() {
            match ch {
                '\\' if self.1 => self.0.write_str("\\\\")?,
                '"' if self.1 => self.0.write_str("\\\"")?,
                '\n' => self.0.write_str("\\n")?,
                '\r' => self.0.write_str("\\r")?,
                '\t' => self.0.write_str("\\t")?,
                ch if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') => {
                    write!(self.0, "\\u{{{:X}}}", u32::from(ch))?;
                }
                ch => self.0.write_char(ch)?,
            }
        }
        Ok(())
    }
}

impl<T: Display> Display for LogText<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SPAN_TAG}{}\"", self.kind.tag())?;
        write!(Escaped(f, true), "{}", self.value)?;
        f.write_char('"')
    }
}

/// Decodes the content between the quotes of a log-text span.
/// Rejects malformed escapes and characters that must have been escaped.
pub fn unescape(value: &str) -> Option<String> {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next()? {
                '\\' => output.push('\\'),
                '"' => output.push('"'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                'u' => {
                    if chars.next()? != '{' {
                        return None;
                    }
                    let mut code = 0u32;
                    let mut digits = 0;
                    loop {
                        let ch = chars.next()?;
                        if ch == '}' {
                            break;
                        }
                        digits += 1;
                        if digits > 6 {
                            return None;
                        }
                        code = code.checked_mul(16)?.checked_add(ch.to_digit(16)?)?;
                    }
                    if digits == 0 {
                        return None;
                    }
                    output.push(char::from_u32(code)?);
                }
                _ => return None,
            },
            '"' => return None,
            ch if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') => return None,
            ch => output.push(ch),
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unicode_scalar_round_trips_without_raw_line_breaks() {
        let original: String = (0..=0x10ffff).filter_map(char::from_u32).collect();
        let encoded = title(&original).to_string();
        let content = encoded
            .strip_prefix("@lwm:title\"")
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert!(!content
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}')));
        assert_eq!(unescape(content).as_deref(), Some(original.as_str()));
    }

    #[test]
    fn malformed_escape_and_raw_delimiters_are_rejected() {
        for input in [
            "\\",
            "\\q",
            "\\u{}",
            "\\u{110000}",
            "\\u{D800}",
            "\\u{1234567}",
            "\\u{1",
            "\"",
            "\n",
            "\u{2028}",
        ] {
            assert_eq!(unescape(input), None, "accepted {input:?}");
        }
    }
}
