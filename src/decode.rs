//! Lexeme decoding for resolver layers: string unescaping and number conversion.
//!
//! The parser itself never decodes — the CST keeps raw spans. This module owns the inverse of
//! the token shapes the rules layer validates, so resolver crates (and superset languages
//! reusing the JSON escapes) share one implementation instead of each rewriting surrogate-pair
//! combination and number classification.
//!
//! The functions are total over arbitrary input: tokens produced by this crate's parser have
//! already had their shape validated and cannot fail with [`ErrorKind::MalformedToken`],
//! but the functions are public and re-check shape instead of trusting their caller.

use std::error;
use std::fmt;

/// Policy for lone UTF-16 surrogate escapes (e.g. `"\uD800"` without a paired low surrogate).
///
/// RFC 8259 §8.2 notes that unpaired surrogate escapes yield unpredictable, often fatal results
/// across implementations; a resolver must therefore choose a policy up front.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LoneSurrogates {
    /// Reject the string with [`ErrorKind::LoneSurrogate`]. The strict default.
    #[default]
    Error,
    /// Replace each lone surrogate with U+FFFD REPLACEMENT CHARACTER.
    Replace,
}

/// A decode failure, carrying the byte offset inside the raw lexeme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    /// Byte offset of the offending content, relative to the start of the raw lexeme.
    pub offset: usize,
    /// What went wrong.
    pub kind: ErrorKind,
}

/// Categories of decode failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The raw bytes are not a lexically valid token of the expected shape.
    ///
    /// Tokens produced by this crate's parser never trigger this; it exists because the decode
    /// functions accept arbitrary byte slices.
    MalformedToken,
    /// A UTF-16 surrogate escape without its pair, under [`LoneSurrogates::Error`].
    LoneSurrogate {
        /// The unpaired code unit.
        unit: u16,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ErrorKind::MalformedToken => {
                write!(
                    formatter,
                    "malformed token content at lexeme byte {}",
                    self.offset
                )
            }
            ErrorKind::LoneSurrogate { unit } => {
                write!(
                    formatter,
                    "lone surrogate escape \\u{unit:04X} at lexeme byte {}",
                    self.offset
                )
            }
        }
    }
}

impl error::Error for Error {}

/// An unpaired surrogate reported by [`Utf16Decoder`]; the caller adds the lexeme offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoneSurrogate {
    /// The unpaired code unit.
    pub unit: u16,
    /// Whether the lone unit was the previously held high surrogate (as opposed to the unit
    /// just pushed). Callers use this to blame the right escape sequence.
    pub was_pending: bool,
}

/// Characters completed by one [`Utf16Decoder::push`].
///
/// Two characters appear when a held high surrogate turns out lone (replaced under
/// [`LoneSurrogates::Replace`]) and the pushed unit is an ordinary scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utf16Step {
    /// Nothing ready yet (a high surrogate is now held).
    None,
    /// One character completed.
    One(char),
    /// Two characters completed, in order.
    Two(char, char),
}

/// Streaming UTF-16 code-unit decoder with surrogate-pair combination.
///
/// Escape walkers feed `\uXXXX` units through [`Utf16Decoder::push`] and flush before emitting
/// any literal (non-escape) content and at end of input. Keeping the pairing state in one place
/// lets superset languages (JSON5 shares JSON's `\uXXXX` escapes) reuse the exact pairing and
/// policy behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct Utf16Decoder {
    pending: Option<u16>,
}

impl Utf16Decoder {
    /// Creates a decoder with no pending surrogate.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { pending: None }
    }

    /// Returns whether a high surrogate is currently held.
    #[inline]
    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Feeds one code unit, returning the characters completed by it.
    ///
    /// A high surrogate is held until its pair arrives. A low surrogate completes the held pair,
    /// or is lone without one; a following high surrogate or ordinary unit makes the held one
    /// lone. Lone surrogates resolve per `policy`.
    pub fn push(&mut self, unit: u16, policy: LoneSurrogates) -> Result<Utf16Step, LoneSurrogate> {
        match (self.pending.take(), unit) {
            (None, 0xD800..=0xDBFF) => {
                self.pending = Some(unit);
                Ok(Utf16Step::None)
            }
            (None, 0xDC00..=0xDFFF) => resolve_lone(unit, false, policy).map(Utf16Step::One),
            (None, _) => Ok(Utf16Step::One(scalar_from_unit(unit))),
            (Some(high), 0xDC00..=0xDFFF) => Ok(Utf16Step::One(combined_surrogates(high, unit))),
            (Some(high), 0xD800..=0xDBFF) => {
                self.pending = Some(unit);
                resolve_lone(high, true, policy).map(Utf16Step::One)
            }
            (Some(high), _) => {
                let replaced = resolve_lone(high, true, policy)?;
                Ok(Utf16Step::Two(replaced, scalar_from_unit(unit)))
            }
        }
    }

    /// Flushes a held high surrogate, if any, per `policy`.
    ///
    /// Call before emitting literal content and at end of input.
    pub fn flush(&mut self, policy: LoneSurrogates) -> Result<Option<char>, LoneSurrogate> {
        match self.pending.take() {
            Some(unit) => resolve_lone(unit, true, policy).map(Some),
            None => Ok(None),
        }
    }
}

/// Converts a non-surrogate UTF-16 code unit to its character.
fn scalar_from_unit(unit: u16) -> char {
    match char::from_u32(u32::from(unit)) {
        Some(scalar) => scalar,
        // Only surrogate units (handled by the callers' match arms) are invalid BMP scalars.
        None => unreachable!("non-surrogate BMP code unit is a valid scalar"),
    }
}

/// Combines a matched surrogate pair into its supplementary-plane character.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "the matched surrogate ranges bound the arithmetic below overflow"
)]
fn combined_surrogates(high: u16, low: u16) -> char {
    let value = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);

    match char::from_u32(value) {
        Some(scalar) => scalar,
        // Combined pairs land in 0x10000..=0x10FFFF, all valid scalars.
        None => unreachable!("combined surrogate pair is a valid supplementary scalar"),
    }
}

/// Resolves one lone surrogate per policy.
const fn resolve_lone(
    unit: u16,
    was_pending: bool,
    policy: LoneSurrogates,
) -> Result<char, LoneSurrogate> {
    match policy {
        LoneSurrogates::Error => Err(LoneSurrogate { unit, was_pending }),
        LoneSurrogates::Replace => Ok('\u{FFFD}'),
    }
}

/// Decodes a raw RFC 8259 string lexeme — both quotes included — into its owned text.
///
/// Handles the §7 escapes (`\" \\ \/ \b \f \n \r \t \uXXXX`) and combines UTF-16 surrogate
/// pairs; lone surrogates resolve per `policy`. Errors carry the byte offset inside `raw` of the
/// backslash of the offending escape.
pub fn unescape_string(raw: &[u8], policy: LoneSurrogates) -> Result<String, Error> {
    let text = validated_quoted(raw, b'"')?;
    let inner_end = raw.len().saturating_sub(1);
    let mut walker = EscapeWalker::new(text, policy);
    let mut index = 1; // past the opening quote

    while index < inner_end {
        if raw.get(index).copied() != Some(b'\\') {
            index = index.saturating_add(1);
            continue;
        }

        walker.take_run_until(index)?;

        let escape_offset = index;
        let escaped = raw.get(index.saturating_add(1)).copied();
        index = index.saturating_add(2);

        let simple = match escaped {
            Some(b'"') => Some('"'),
            Some(b'\\') => Some('\\'),
            Some(b'/') => Some('/'),
            Some(b'b') => Some('\u{08}'),
            Some(b'f') => Some('\u{0C}'),
            Some(b'n') => Some('\n'),
            Some(b'r') => Some('\r'),
            Some(b't') => Some('\t'),
            Some(b'u') => None,
            _ => {
                return Err(Error {
                    offset: escape_offset,
                    kind: ErrorKind::MalformedToken,
                });
            }
        };

        if let Some(ch) = simple {
            walker.push_char(ch, escape_offset)?;
        } else {
            let unit = parse_hex4(raw, index).ok_or(Error {
                offset: escape_offset,
                kind: ErrorKind::MalformedToken,
            })?;
            index = index.saturating_add(4);
            walker.push_unit(unit, escape_offset)?;
        }

        walker.resume_run_at(index);
    }

    walker.take_run_until(inner_end)?;
    walker.finish(inner_end)
}

/// The escape-walking state shared between literal runs and decoded escapes.
struct EscapeWalker<'text> {
    text: &'text str,
    policy: LoneSurrogates,
    out: String,
    decoder: Utf16Decoder,
    /// Offset of the `\` that produced the held high surrogate, when one is held.
    pending_offset: usize,
    run_start: usize,
}

impl<'text> EscapeWalker<'text> {
    fn new(text: &'text str, policy: LoneSurrogates) -> Self {
        Self {
            text,
            policy,
            out: String::with_capacity(text.len()),
            decoder: Utf16Decoder::new(),
            pending_offset: 0,
            run_start: 1,
        }
    }

    /// Emits the literal run `[run_start, end)`, flushing any held surrogate first.
    fn take_run_until(&mut self, end: usize) -> Result<(), Error> {
        if self.run_start >= end {
            return Ok(());
        }

        self.flush_decoder()?;

        let slice = self.text.get(self.run_start..end).ok_or(Error {
            offset: self.run_start,
            kind: ErrorKind::MalformedToken,
        })?;
        self.out.push_str(slice);
        self.run_start = end;

        Ok(())
    }

    /// Restarts literal-run tracking after an escape.
    fn resume_run_at(&mut self, index: usize) {
        self.run_start = index;
    }

    /// Emits one decoded simple-escape character, flushing any held surrogate first.
    fn push_char(&mut self, ch: char, _escape_offset: usize) -> Result<(), Error> {
        self.flush_decoder()?;
        self.out.push(ch);

        Ok(())
    }

    /// Feeds one `\uXXXX` code unit from the escape at `escape_offset`.
    fn push_unit(&mut self, unit: u16, escape_offset: usize) -> Result<(), Error> {
        match self.decoder.push(unit, self.policy) {
            Ok(Utf16Step::None) => {
                self.pending_offset = escape_offset;
            }
            Ok(Utf16Step::One(ch)) => self.out.push(ch),
            Ok(Utf16Step::Two(first, second)) => {
                self.out.push(first);
                self.out.push(second);
            }
            Err(LoneSurrogate { unit, was_pending }) => {
                return Err(Error {
                    offset: if was_pending {
                        self.pending_offset
                    } else {
                        escape_offset
                    },
                    kind: ErrorKind::LoneSurrogate { unit },
                });
            }
        }

        if self.decoder.has_pending() {
            self.pending_offset = escape_offset;
        }

        Ok(())
    }

    /// Flushes a held surrogate, blaming its own escape on error.
    fn flush_decoder(&mut self) -> Result<(), Error> {
        match self.decoder.flush(self.policy) {
            Ok(Some(ch)) => {
                self.out.push(ch);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(LoneSurrogate { unit, .. }) => Err(Error {
                offset: self.pending_offset,
                kind: ErrorKind::LoneSurrogate { unit },
            }),
        }
    }

    /// Finishes decoding, flushing any held surrogate.
    fn finish(mut self, _end: usize) -> Result<String, Error> {
        self.flush_decoder()?;

        Ok(self.out)
    }
}

/// Validates the quoting shape and UTF-8 of a raw string lexeme, returning it as text.
fn validated_quoted(raw: &[u8], quote: u8) -> Result<&str, Error> {
    if raw.len() < 2 || raw.first() != Some(&quote) || raw.last() != Some(&quote) {
        return Err(Error {
            offset: 0,
            kind: ErrorKind::MalformedToken,
        });
    }

    core::str::from_utf8(raw).map_err(|error| Error {
        offset: error.valid_up_to(),
        kind: ErrorKind::MalformedToken,
    })
}

/// Parses four ASCII hex digits at `index` into a UTF-16 code unit.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "digit values are 0..16 and four nibbles fit u16 exactly"
)]
fn parse_hex4(raw: &[u8], index: usize) -> Option<u16> {
    let digits = raw.get(index..index.saturating_add(4))?;
    let mut unit: u16 = 0;

    for &digit in digits {
        let value = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return None,
        };
        unit = (unit << 4) | u16::from(value);
    }

    Some(unit)
}

/// An owned numeric value converted from a JSON number lexeme, integer-preserving where the
/// value fits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// A non-negative integer that fits `u64`.
    U64(u64),
    /// A negative integer that fits `i64`.
    I64(i64),
    /// Everything else: fractions, exponents, `-0`, and integers beyond 64 bits.
    F64(f64),
}

impl Number {
    /// The value as `f64`, converting integers; may round for magnitudes beyond 2^53.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            #[expect(
                clippy::as_conversions,
                clippy::cast_precision_loss,
                reason = "documented rounding conversion"
            )]
            Self::U64(value) => value as f64,
            #[expect(
                clippy::as_conversions,
                clippy::cast_precision_loss,
                reason = "documented rounding conversion"
            )]
            Self::I64(value) => value as f64,
            Self::F64(value) => value,
        }
    }

    /// The value as `u64` when it is exactly a non-negative integer.
    #[must_use]
    pub const fn as_u64(self) -> Option<u64> {
        match self {
            Self::U64(value) => Some(value),
            Self::I64(_) | Self::F64(_) => None,
        }
    }

    /// The value as `i64` when it is exactly an integer in `i64` range.
    #[must_use]
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Self::U64(value) => i64::try_from(value).ok(),
            Self::I64(value) => Some(value),
            Self::F64(_) => None,
        }
    }
}

/// Converts a raw RFC 8259 number lexeme into an owned [`Number`].
///
/// Integers keep their integer identity where they fit (`U64` for non-negative, `I64` for
/// negative); everything else converts to `f64` — including `-0`, which stays `-0.0` to preserve
/// its sign, and integers beyond 64 bits, which may round (magnitudes beyond `f64` range become
/// infinite). The raw text stays available through the CST for consumers needing arbitrary
/// precision.
pub fn parse_number(raw: &[u8]) -> Result<Number, Error> {
    let text = core::str::from_utf8(raw).map_err(|error| Error {
        offset: error.valid_up_to(),
        kind: ErrorKind::MalformedToken,
    })?;
    let malformed = Error {
        offset: 0,
        kind: ErrorKind::MalformedToken,
    };

    if text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        return text.parse::<f64>().map(Number::F64).map_err(|_| malformed);
    }

    if let Some(digits) = text.strip_prefix('-') {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed);
        }

        return match text.parse::<i64>() {
            // `-0` preserves its sign as a float; parsing the text keeps the minus.
            Ok(0) => text.parse::<f64>().map(Number::F64).map_err(|_| malformed),
            Ok(value) => Ok(Number::I64(value)),
            // Negative magnitude beyond i64: fall back to f64.
            Err(_) => text.parse::<f64>().map(Number::F64).map_err(|_| malformed),
        };
    }

    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed);
    }

    match text.parse::<u64>() {
        Ok(value) => Ok(Number::U64(value)),
        // Magnitude beyond u64: fall back to f64.
        Err(_) => text.parse::<f64>().map(Number::F64).map_err(|_| malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unescape(raw: &str) -> Result<String, Error> {
        unescape_string(raw.as_bytes(), LoneSurrogates::Error)
    }

    fn unescape_replacing(raw: &str) -> Result<String, Error> {
        unescape_string(raw.as_bytes(), LoneSurrogates::Replace)
    }

    #[test]
    fn plain_strings_decode_verbatim() -> Result<(), Error> {
        assert_eq!(unescape(r#""hello""#)?, "hello");
        assert_eq!(unescape(r#""""#)?, "");
        assert_eq!(unescape("\"caf\u{E9} \u{1F600}\"")?, "caf\u{E9} \u{1F600}");

        Ok(())
    }

    #[test]
    fn simple_escapes_decode() -> Result<(), Error> {
        assert_eq!(
            unescape(r#""\"\\\/\b\f\n\r\t""#)?,
            "\"\\/\u{08}\u{0C}\n\r\t"
        );

        Ok(())
    }

    #[test]
    fn unicode_escapes_decode() -> Result<(), Error> {
        assert_eq!(unescape(r#""Aé€""#)?, "A\u{E9}\u{20AC}");
        assert_eq!(unescape(r#"" ""#)?, "\u{0}");

        Ok(())
    }

    #[test]
    fn surrogate_pairs_combine() -> Result<(), Error> {
        assert_eq!(unescape(r#""\ud83d\ude00""#)?, "\u{1F600}");
        assert_eq!(unescape(r#""x\uD83D\uDE00y""#)?, "x\u{1F600}y");

        Ok(())
    }

    #[test]
    fn lone_surrogates_error_with_offsets() {
        // Bytes: 0 `"`, 1 the backslash of the lone escape.
        let Err(error) = unescape(r#""\ud800""#) else {
            panic!("expected a lone surrogate error");
        };
        assert_eq!(error.kind, ErrorKind::LoneSurrogate { unit: 0xD800 });
        assert_eq!(error.offset, 1);

        // A lone low surrogate errors at its own escape.
        let Err(error) = unescape(r#""a\udc00""#) else {
            panic!("expected a lone surrogate error");
        };
        assert_eq!(error.kind, ErrorKind::LoneSurrogate { unit: 0xDC00 });
        assert_eq!(error.offset, 2);

        // A high surrogate followed by literal content is blamed at the high's escape.
        let Err(error) = unescape(r#""\ud800abc""#) else {
            panic!("expected a lone surrogate error");
        };
        assert_eq!(error.kind, ErrorKind::LoneSurrogate { unit: 0xD800 });
        assert_eq!(error.offset, 1);

        // A high surrogate followed by a non-surrogate escape unit.
        let Err(error) = unescape(r#""\ud800\u0041""#) else {
            panic!("expected a lone surrogate error");
        };
        assert_eq!(error.kind, ErrorKind::LoneSurrogate { unit: 0xD800 });
        assert_eq!(error.offset, 1);
    }

    #[test]
    fn lone_surrogates_replace_under_policy() -> Result<(), Error> {
        assert_eq!(unescape_replacing(r#""\ud800""#)?, "\u{FFFD}");
        assert_eq!(unescape_replacing(r#""\udc00""#)?, "\u{FFFD}");
        assert_eq!(unescape_replacing(r#""\ud800abc""#)?, "\u{FFFD}abc");
        assert_eq!(unescape_replacing(r#""\ud800\u0041""#)?, "\u{FFFD}A");
        assert_eq!(
            unescape_replacing(r#""\ud800\ud83d\ude00""#)?,
            "\u{FFFD}\u{1F600}"
        );

        Ok(())
    }

    #[test]
    fn malformed_shapes_are_rejected() {
        for raw in [&b"hello"[..], b"\"open", b"x\"", b"\""] {
            let Err(error) = unescape_string(raw, LoneSurrogates::Error) else {
                panic!("expected a malformed-token error for {raw:?}");
            };
            assert_eq!(error.kind, ErrorKind::MalformedToken);
        }

        let Err(error) = unescape(r#""\q""#) else {
            panic!("expected a malformed-token error");
        };
        assert_eq!(error.kind, ErrorKind::MalformedToken);
        assert_eq!(error.offset, 1);

        let Err(error) = unescape(r#""\u12g4""#) else {
            panic!("expected a malformed-token error");
        };
        assert_eq!(error.kind, ErrorKind::MalformedToken);
        assert_eq!(error.offset, 1);
    }

    #[test]
    fn numbers_preserve_integer_identity() -> Result<(), Error> {
        assert_eq!(parse_number(b"0")?, Number::U64(0));
        assert_eq!(parse_number(b"42")?, Number::U64(42));
        assert_eq!(
            parse_number(b"18446744073709551615")?,
            Number::U64(u64::MAX)
        );
        assert_eq!(parse_number(b"-17")?, Number::I64(-17));
        assert_eq!(
            parse_number(b"-9223372036854775808")?,
            Number::I64(i64::MIN)
        );

        Ok(())
    }

    #[test]
    fn negative_zero_preserves_its_sign() -> Result<(), Error> {
        let Number::F64(value) = parse_number(b"-0")? else {
            panic!("expected -0 to convert as a float");
        };
        assert_eq!(value.to_bits(), (-0.0f64).to_bits());

        Ok(())
    }

    #[test]
    fn fractions_and_exponents_convert_to_f64() -> Result<(), Error> {
        assert_eq!(parse_number(b"3.25")?, Number::F64(3.25));
        assert_eq!(parse_number(b"-0.5")?, Number::F64(-0.5));
        assert_eq!(parse_number(b"1e10")?, Number::F64(1e10));
        assert_eq!(parse_number(b"2.5E-3")?, Number::F64(2.5e-3));

        Ok(())
    }

    #[test]
    fn oversized_integers_fall_back_to_f64() -> Result<(), Error> {
        assert_eq!(
            parse_number(b"18446744073709551616")?,
            Number::F64(18_446_744_073_709_551_616.0)
        );
        assert_eq!(
            parse_number(b"-9223372036854775809")?,
            Number::F64(-9_223_372_036_854_775_809.0)
        );

        Ok(())
    }

    #[test]
    fn malformed_numbers_are_rejected() {
        for raw in [&b""[..], b"-", b"+1", b"abc", b"0x10"] {
            let Err(error) = parse_number(raw) else {
                panic!("expected a malformed-token error for {raw:?}");
            };
            assert_eq!(error.kind, ErrorKind::MalformedToken);
        }
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "small integers convert exactly")]
    fn number_accessors_convert() {
        assert_eq!(Number::U64(7).as_u64(), Some(7));
        assert_eq!(Number::U64(7).as_i64(), Some(7));
        assert_eq!(Number::U64(u64::MAX).as_i64(), None);
        assert_eq!(Number::I64(-7).as_i64(), Some(-7));
        assert_eq!(Number::I64(-7).as_u64(), None);
        assert_eq!(Number::F64(1.5).as_u64(), None);
        assert_eq!(Number::U64(2).as_f64(), 2.0);
        assert_eq!(Number::I64(-2).as_f64(), -2.0);
    }
}
