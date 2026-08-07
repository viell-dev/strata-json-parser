//! The strict RFC 8259 parser front: token rules in, CST out.
//!
//! [`JsonParser::new`] is the fully strict path. [`JsonParser::builder`] exposes the two places
//! where RFC 8259 explicitly grants parsers latitude — tolerating a leading byte-order mark
//! (§8.1) and the nesting-depth implementation limit (§9) — plus `with_*` hooks to customize or
//! replace the lower layers: the token rules and the lexer configuration. The parser hands the
//! lexer its rule set at construction, which is what lets a superset language swap in an
//! extended rule set while reusing the machinery.
//!
//! The grammar itself lives in [`crate::grammar`]; this module implements the JSON
//! [`Syntax`] over it and owns the JSON-specific error type and options.

use crate::cst;
use crate::grammar::{self, Lexed, Punct, Syntax, ValueRole};
use std::error::Error;
use std::fmt;
use std::io::{Cursor, Read};
use strata_json_rules::{JsonToken, StrictRuleSet, strict_rule_set};
use strata_lexer::{LexError, LexerConfig, MatcherSet, Position, Token};

/// Classification of a retained trivia piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriviaKind {
    /// A run of insignificant whitespace (RFC 8259 §2).
    Whitespace,
    /// A tolerated leading byte-order mark (RFC 8259 §8.1, `allow_bom`).
    Bom,
}

impl cst::TriviaClass for TriviaKind {
    #[inline]
    fn is_bom(self) -> bool {
        matches!(self, Self::Bom)
    }
}

/// A retained piece of grammatically insignificant JSON source content.
pub type Trivia = cst::Trivia<TriviaKind>;
/// A significant JSON token in the CST.
pub type SyntaxToken = cst::SyntaxToken<JsonToken, TriviaKind>;
/// A JSON value node.
pub type Value = cst::Value<JsonToken, TriviaKind>;
/// A JSON array node.
pub type ArrayNode = cst::ArrayNode<JsonToken, TriviaKind>;
/// One JSON array element.
pub type Element = cst::Element<JsonToken, TriviaKind>;
/// A JSON object node.
pub type ObjectNode = cst::ObjectNode<JsonToken, TriviaKind>;
/// One JSON object member.
pub type Member = cst::Member<JsonToken, TriviaKind>;
/// A parsed JSON document.
pub type Document = cst::Document<JsonToken, TriviaKind>;

/// The default nesting-depth limit.
///
/// RFC 8259 §9 permits parsers to set limits on the depth of nesting; this is the crate's
/// default for the maximum number of nested arrays/objects. Override it with
/// [`JsonParserBuilder::max_depth`]. The limit also bounds the parser's recursion, so raising it
/// substantially trades stack headroom for depth.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// A parse failure, carrying the position at which it was observed.
#[derive(Debug)]
pub struct JsonError {
    /// Position of the offending byte or token start.
    pub position: Position,
    /// What went wrong.
    pub kind: JsonErrorKind,
}

/// Categories of parse failures.
#[derive(Debug)]
pub enum JsonErrorKind {
    /// The lexer failed: I/O, an unrecognized byte, or a token-shape violation (invalid string
    /// escapes, unescaped control characters, number-grammar violations, invalid UTF-8) reported
    /// at the offending byte.
    Lex(LexError),
    /// A token of an unexpected kind where the grammar required something else.
    Expected {
        /// What the grammar required here.
        expected: &'static str,
        /// The token kind actually found.
        found: JsonToken,
    },
    /// The input ended where the grammar required more.
    UnexpectedEof {
        /// What the grammar required here.
        expected: &'static str,
    },
    /// A significant token followed the complete top-level value.
    TrailingContent {
        /// The token kind found after the value.
        found: JsonToken,
    },
    /// The input starts with a byte-order mark and the parser was not configured to tolerate it.
    ///
    /// RFC 8259 §8.1 forbids adding a BOM but permits implementations to ignore one; enable
    /// [`JsonParserBuilder::allow_bom`] to opt into that permission.
    UnexpectedBom,
    /// Nesting exceeded the configured depth limit (RFC 8259 §9 implementation limit).
    DepthLimitExceeded {
        /// The configured limit.
        limit: usize,
    },
}

impl fmt::Display for JsonErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => write!(formatter, "{error}"),
            Self::Expected { expected, found } => {
                write!(formatter, "expected {expected}, found {}", found.describe())
            }
            Self::UnexpectedEof { expected } => {
                write!(formatter, "unexpected end of input, expected {expected}")
            }
            Self::TrailingContent { found } => write!(
                formatter,
                "unexpected {} after the top-level value",
                found.describe()
            ),
            Self::UnexpectedBom => formatter.write_str(
                "leading byte-order mark (enable allow_bom to tolerate it per RFC 8259 \u{a7}8.1)",
            ),
            Self::DepthLimitExceeded { limit } => {
                write!(formatter, "nesting depth limit of {limit} exceeded")
            }
        }
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            // The lexer's message already carries its byte position.
            JsonErrorKind::Lex(error) => write!(formatter, "{error}"),
            kind => write!(
                formatter,
                "{kind} at line {}, column {} (byte {})",
                self.position.line(),
                self.position.column(),
                self.position.byte()
            ),
        }
    }
}

impl Error for JsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            JsonErrorKind::Lex(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for JsonError {
    fn from(error: LexError) -> Self {
        Self {
            position: error.position,
            kind: JsonErrorKind::Lex(error),
        }
    }
}

/// The JSON [`Syntax`]: one parse's classification policy over [`JsonToken`].
#[derive(Debug, Clone, Copy)]
struct JsonSyntax {
    allow_bom: bool,
}

impl Syntax for JsonSyntax {
    type Lex = JsonToken;
    type Kind = JsonToken;
    type Trivia = TriviaKind;
    type Error = JsonError;

    fn eof_lex(&self) -> JsonToken {
        JsonToken::Eof
    }

    fn classify(
        &self,
        token: &Token<JsonToken>,
    ) -> Result<Lexed<JsonToken, TriviaKind>, JsonError> {
        match token.kind {
            JsonToken::Whitespace => Ok(Lexed::Trivia(TriviaKind::Whitespace)),
            JsonToken::Bom => {
                if self.allow_bom {
                    Ok(Lexed::Trivia(TriviaKind::Bom))
                } else {
                    Err(JsonError {
                        position: token.start(),
                        kind: JsonErrorKind::UnexpectedBom,
                    })
                }
            }
            JsonToken::Eof => Ok(Lexed::Eof),
            kind => Ok(Lexed::Significant(kind)),
        }
    }

    fn value_role(&self, kind: JsonToken) -> Option<ValueRole> {
        match kind {
            JsonToken::LeftBrace => Some(ValueRole::ObjectOpen),
            JsonToken::LeftBracket => Some(ValueRole::ArrayOpen),
            JsonToken::String => Some(ValueRole::String),
            JsonToken::Number => Some(ValueRole::Number),
            JsonToken::True => Some(ValueRole::True),
            JsonToken::False => Some(ValueRole::False),
            JsonToken::Null => Some(ValueRole::Null),
            _ => None,
        }
    }

    fn punct(&self, kind: JsonToken) -> Option<Punct> {
        match kind {
            JsonToken::Colon => Some(Punct::Colon),
            JsonToken::Comma => Some(Punct::Comma),
            JsonToken::RightBrace => Some(Punct::ObjectClose),
            JsonToken::RightBracket => Some(Punct::ArrayClose),
            _ => None,
        }
    }

    fn is_member_name(&self, kind: JsonToken) -> bool {
        kind == JsonToken::String
    }

    fn value_expectation(&self) -> &'static str {
        "a JSON value"
    }

    fn member_name_expectation(&self) -> &'static str {
        "an object member name (a string)"
    }

    fn expected(&self, position: Position, expected: &'static str, found: JsonToken) -> JsonError {
        JsonError {
            position,
            kind: JsonErrorKind::Expected { expected, found },
        }
    }

    fn unexpected_eof(&self, position: Position, expected: &'static str) -> JsonError {
        JsonError {
            position,
            kind: JsonErrorKind::UnexpectedEof { expected },
        }
    }

    fn trailing_content(&self, position: Position, found: JsonToken) -> JsonError {
        JsonError {
            position,
            kind: JsonErrorKind::TrailingContent { found },
        }
    }

    fn depth_exceeded(&self, position: Position, limit: usize) -> JsonError {
        JsonError {
            position,
            kind: JsonErrorKind::DepthLimitExceeded { limit },
        }
    }
}

/// A reusable, strict RFC 8259 parser.
///
/// `M` is the token rule set handed to the lexer for every parse; it defaults to the strict
/// RFC 8259 rules. The parser clones the rules into a fresh scanner per [`JsonParser::parse`]
/// call, so one configured parser can serve many inputs.
#[derive(Debug, Clone)]
pub struct JsonParser<M = StrictRuleSet> {
    rules: M,
    allow_bom: bool,
    max_depth: usize,
    lexer_config: LexerConfig,
}

impl JsonParser<StrictRuleSet> {
    /// Creates a fully strict parser: strict rules, no BOM tolerance,
    /// [`DEFAULT_MAX_DEPTH`] nesting.
    pub fn new() -> Self {
        Self {
            rules: strict_rule_set(),
            allow_bom: false,
            max_depth: DEFAULT_MAX_DEPTH,
            lexer_config: LexerConfig::new(),
        }
    }

    /// Starts a builder for customizing spec-permitted options or replacing lower layers.
    pub fn builder() -> JsonParserBuilder<StrictRuleSet> {
        JsonParserBuilder::new()
    }
}

impl Default for JsonParser<StrictRuleSet> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> JsonParser<M>
where
    M: MatcherSet<Kind = JsonToken> + Clone,
{
    /// Parses a complete JSON text from `reader` into a [`Document`].
    ///
    /// Any RFC 8259 value is a valid top-level document (§2). The entire input must be consumed:
    /// trailing non-whitespace content is an error. On success the consumed source bytes are
    /// retained in the document's source store, keeping the CST zero-copy.
    pub fn parse<R: Read>(&self, reader: R) -> Result<Document, JsonError> {
        let syntax = JsonSyntax {
            allow_bom: self.allow_bom,
        };

        grammar::parse_document(
            reader,
            self.rules.clone(),
            &syntax,
            self.max_depth,
            self.lexer_config,
        )
    }

    /// Parses a complete JSON text from a string slice.
    pub fn parse_str(&self, input: &str) -> Result<Document, JsonError> {
        self.parse(Cursor::new(input.as_bytes()))
    }

    /// Parses a complete JSON text from a byte slice. The bytes must be UTF-8 (RFC 8259 §8.1);
    /// violations inside string tokens are lexical errors, and any non-ASCII byte outside a
    /// string matches no rule.
    pub fn parse_bytes(&self, input: &[u8]) -> Result<Document, JsonError> {
        self.parse(Cursor::new(input))
    }

    /// The token rule set handed to the lexer at each parse.
    pub const fn rules(&self) -> &M {
        &self.rules
    }

    /// Whether a leading byte-order mark is tolerated (RFC 8259 §8.1).
    pub const fn allow_bom(&self) -> bool {
        self.allow_bom
    }

    /// The configured nesting-depth limit (RFC 8259 §9).
    pub const fn max_depth(&self) -> usize {
        self.max_depth
    }
}

/// Builder for [`JsonParser`], strict by default.
#[derive(Debug, Clone)]
pub struct JsonParserBuilder<M = StrictRuleSet> {
    rules: M,
    allow_bom: bool,
    max_depth: usize,
    lexer_config: LexerConfig,
}

impl JsonParserBuilder<StrictRuleSet> {
    /// Starts from the fully strict defaults.
    pub fn new() -> Self {
        Self {
            rules: strict_rule_set(),
            allow_bom: false,
            max_depth: DEFAULT_MAX_DEPTH,
            lexer_config: LexerConfig::new(),
        }
    }
}

impl Default for JsonParserBuilder<StrictRuleSet> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> JsonParserBuilder<M> {
    /// Replaces the token rule set handed to the lexer.
    ///
    /// This is the rules-layer composition point: tests can mock the rules, and adjusted rule
    /// sets can be swapped in without touching the grammar driver. The set must still produce
    /// [`JsonToken`] kinds; a superset *language* instead implements its own
    /// [`Syntax`](crate::grammar::Syntax) over its own kinds, reusing the rule structs from
    /// `strata-json-rules` via `map_kind`.
    pub fn with_rules<N>(self, rules: N) -> JsonParserBuilder<N>
    where
        N: MatcherSet<Kind = JsonToken> + Clone,
    {
        JsonParserBuilder {
            rules,
            allow_bom: self.allow_bom,
            max_depth: self.max_depth,
            lexer_config: self.lexer_config,
        }
    }

    /// Replaces the lexer configuration (reader capacities, position tracking, ...).
    ///
    /// The parser always disables buffer compaction (`compact_after = None`) at parse time: the
    /// consumed bytes must survive until they are parked into the document's source store.
    #[must_use]
    pub const fn with_lexer_config(mut self, config: LexerConfig) -> Self {
        self.lexer_config = config;
        self
    }

    /// Tolerates one leading byte-order mark, retaining it as trivia.
    ///
    /// Off by default. RFC 8259 §8.1: a JSON text must not begin with a BOM, but implementations
    /// "MAY ignore the presence of a byte order mark rather than treating it as an error". With
    /// the option off, a leading BOM is [`JsonErrorKind::UnexpectedBom`]; a BOM anywhere else in
    /// the input is always an error.
    #[must_use]
    pub const fn allow_bom(mut self, allow: bool) -> Self {
        self.allow_bom = allow;
        self
    }

    /// Sets the nesting-depth limit.
    ///
    /// RFC 8259 §9: "An implementation may set limits on the maximum depth of nesting." The
    /// limit counts nested arrays/objects — with a limit of 1, `[1]` parses and `[[1]]` does
    /// not. It also bounds the parser's recursion depth; see [`DEFAULT_MAX_DEPTH`].
    #[must_use]
    pub const fn max_depth(mut self, limit: usize) -> Self {
        self.max_depth = limit;
        self
    }

    /// Finishes the builder.
    pub fn build(self) -> JsonParser<M>
    where
        M: MatcherSet<Kind = JsonToken> + Clone,
    {
        JsonParser {
            rules: self.rules,
            allow_bom: self.allow_bom,
            max_depth: self.max_depth,
            lexer_config: self.lexer_config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_fully_strict() {
        let parser = JsonParser::new();
        assert!(!parser.allow_bom());
        assert_eq!(parser.max_depth(), DEFAULT_MAX_DEPTH);
    }

    #[test]
    fn builder_defaults_match_new() {
        let parser = JsonParser::builder().build();
        assert!(!parser.allow_bom());
        assert_eq!(parser.max_depth(), DEFAULT_MAX_DEPTH);
    }

    #[test]
    fn builder_sets_options() {
        let parser = JsonParser::builder().allow_bom(true).max_depth(4).build();
        assert!(parser.allow_bom());
        assert_eq!(parser.max_depth(), 4);
    }

    #[test]
    fn error_display_includes_position() {
        let Err(error) = JsonParser::new().parse_str("[1 2]") else {
            panic!("expected a parse error");
        };
        let message = error.to_string();
        assert!(message.contains("expected ',' or ']'"), "{message}");
        assert!(message.contains("line 1"), "{message}");
        assert!(message.contains("byte 3"), "{message}");
    }

    #[test]
    fn bom_error_display_mentions_the_option() {
        let Err(error) = JsonParser::new().parse_str("\u{FEFF}1") else {
            panic!("expected a parse error");
        };
        assert!(matches!(error.kind, JsonErrorKind::UnexpectedBom));
        assert!(error.to_string().contains("allow_bom"), "{error}");
    }
}
