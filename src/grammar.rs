//! The shared JSON-family grammar driver.
//!
//! The `value (object | array | scalar)` grammar with comma-separated entries is identical
//! across JSON and its superset languages; what differs is token classification, which token
//! kinds may open a value or name a member, whether trailing commas are permitted, and the
//! concrete error type. The [`Syntax`] trait captures exactly those decision points, and
//! [`parse_document`] is the one recursive-descent driver written against it — a superset parser
//! implements `Syntax` instead of duplicating the driver and the CST.
//!
//! The driver owns trivia folding (every piece of trivia attaches to the next significant token,
//! or to the document end), the nesting-depth limit, the whole-input requirement (content after
//! the root value is an error), and parking the consumed source bytes into the document's
//! [`SourceStore`].

use crate::cst::{ArrayNode, Document, Element, Member, ObjectNode, SyntaxToken, Trivia, Value};
use std::io::Read;
use strata_lexer::{
    LexError, Lexer, LexerConfig, MatcherSet, Position, SourceStore, SpecScanner, Token,
};

/// Classification of one lexer token by a [`Syntax`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lexed<K, T> {
    /// A grammatically significant token of tree kind `K`.
    Significant(K),
    /// A trivia piece of kind `T`, retained and attached to the next significant token.
    Trivia(T),
    /// End of input.
    Eof,
}

/// The grammar role a significant token plays when a value is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRole {
    /// The `null` literal.
    Null,
    /// The `true` literal.
    True,
    /// The `false` literal.
    False,
    /// A number scalar.
    Number,
    /// A string scalar.
    String,
    /// A token opening an object.
    ObjectOpen,
    /// A token opening an array.
    ArrayOpen,
}

/// The structural punctuation roles the driver must recognize mid-production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    /// The name separator (`:`).
    Colon,
    /// The entry separator (`,`).
    Comma,
    /// The object-closing token (`}`).
    ObjectClose,
    /// The array-closing token (`]`).
    ArrayClose,
}

/// The language-specific decision points of the shared grammar driver.
///
/// An implementation is a cheap, borrowed view of one parse's configuration (e.g. whether a BOM
/// is tolerated); [`parse_document`] consults it at every classification and error site. Error
/// values are produced through the trait so every language keeps its own precise error enum —
/// including variants the driver itself never produces.
pub trait Syntax {
    /// The lexer-facing token kind the rule set produces.
    type Lex: Copy + Eq;
    /// The tree-facing token kind stored in the CST.
    type Kind: Copy + Eq;
    /// The trivia kind stored in the CST.
    type Trivia: Copy;
    /// The language's parse error type.
    type Error: From<LexError>;

    /// The end-of-input lexer kind, handed to the scanner.
    fn eof_lex(&self) -> Self::Lex;

    /// Classifies one lexer token as significant, trivia, or end of input.
    ///
    /// Policy checks that reject individual tokens live here — e.g. the strict JSON front
    /// rejects a byte-order-mark token unless configured to tolerate it as trivia.
    fn classify(
        &self,
        token: &Token<Self::Lex>,
    ) -> Result<Lexed<Self::Kind, Self::Trivia>, Self::Error>;

    /// The role `kind` plays where a value is expected; `None` if it cannot start a value.
    fn value_role(&self, kind: Self::Kind) -> Option<ValueRole>;

    /// The structural punctuation role of `kind`, if any.
    fn punct(&self, kind: Self::Kind) -> Option<Punct>;

    /// Whether `kind` may name an object member.
    fn is_member_name(&self, kind: Self::Kind) -> bool;

    /// Diagnostic text for the value expectation (e.g. `"a JSON value"`).
    fn value_expectation(&self) -> &'static str;

    /// Diagnostic text for the member-name expectation.
    fn member_name_expectation(&self) -> &'static str;

    /// Whether a comma directly before a container close is permitted.
    fn allow_trailing_commas(&self) -> bool {
        false
    }

    /// Creates an unexpected-token error.
    fn expected(
        &self,
        position: Position,
        expected: &'static str,
        found: Self::Kind,
    ) -> Self::Error;

    /// Creates an unexpected-end-of-input error.
    fn unexpected_eof(&self, position: Position, expected: &'static str) -> Self::Error;

    /// Creates a trailing-content error for a token after the root value.
    fn trailing_content(&self, position: Position, found: Self::Kind) -> Self::Error;

    /// Creates a nesting-depth error.
    fn depth_exceeded(&self, position: Position, limit: usize) -> Self::Error;
}

/// Parses one complete document from `reader` with the given rules and syntax.
///
/// The entire input must be consumed: significant content after the root value is an error. On
/// success the consumed source bytes are parked into the document's [`SourceStore`], keeping the
/// CST zero-copy. Buffer compaction is disabled for the duration of the parse so those bytes
/// survive until they are parked.
pub fn parse_document<R, M, S>(
    reader: R,
    rules: M,
    syntax: &S,
    max_depth: usize,
    lexer_config: LexerConfig,
) -> Result<Document<S::Kind, S::Trivia>, S::Error>
where
    R: Read,
    M: MatcherSet<Kind = S::Lex>,
    S: Syntax,
{
    // Compaction must stay off so the consumed bytes survive until they are parked into the
    // document's source store.
    let config = lexer_config.with_compact_after(None);
    let scanner = SpecScanner::new(rules, syntax.eof_lex());
    let mut driver = Driver {
        lexer: Lexer::with_config(reader, scanner, config),
        syntax,
        pending: Vec::new(),
        peeked: None,
        max_depth,
    };

    let root = driver.parse_value(0)?;

    match driver.next_significant()? {
        Significant::Token(token) => Err(syntax.trailing_content(token.start(), token.kind)),
        Significant::End { trailing, .. } => {
            let (mut input, _scanner) = driver.lexer.into_parts();
            let mut source = SourceStore::new();
            input.park_consumed_buffer(&mut source)?;

            Ok(Document::new(root, trailing, source))
        }
    }
}

/// A significant step of the token stream: a real token or the end of input.
enum Significant<K, T> {
    Token(SyntaxToken<K, T>),
    End {
        position: Position,
        trailing: Vec<Trivia<T>>,
    },
}

/// The per-parse driver: pulls tokens, folds trivia into the next significant token, and
/// recursively builds the CST.
struct Driver<'syntax, R: Read, M: MatcherSet, S: Syntax<Lex = M::Kind>> {
    lexer: Lexer<R, SpecScanner<M, S::Lex>>,
    syntax: &'syntax S,
    pending: Vec<Trivia<S::Trivia>>,
    peeked: Option<Significant<S::Kind, S::Trivia>>,
    max_depth: usize,
}

impl<R, M, S> Driver<'_, R, M, S>
where
    R: Read,
    M: MatcherSet<Kind = S::Lex>,
    S: Syntax,
{
    /// Returns the next significant token (leading trivia attached) or the end of input
    /// (trailing trivia attached).
    fn next_significant(&mut self) -> Result<Significant<S::Kind, S::Trivia>, S::Error> {
        if let Some(step) = self.peeked.take() {
            return Ok(step);
        }

        loop {
            let token = self.lexer.next_token()?;

            match self.syntax.classify(&token)? {
                Lexed::Trivia(kind) => self.pending.push(Trivia::from_token(kind, &token)),
                Lexed::Significant(kind) => {
                    return Ok(Significant::Token(SyntaxToken::from_token(
                        kind,
                        &token,
                        std::mem::take(&mut self.pending),
                    )));
                }
                Lexed::Eof => {
                    return Ok(Significant::End {
                        position: token.start(),
                        trailing: std::mem::take(&mut self.pending),
                    });
                }
            }
        }
    }

    /// Peeks the structural punctuation role of the next significant token, without consuming
    /// it. `None` covers both end of input and non-punctuation tokens.
    fn peek_punct(&mut self) -> Result<Option<Punct>, S::Error> {
        if self.peeked.is_none() {
            let step = self.next_significant()?;
            self.peeked = Some(step);
        }

        Ok(match &self.peeked {
            Some(Significant::Token(token)) => self.syntax.punct(token.kind),
            _ => None,
        })
    }

    /// Consumes the peeked significant token; only called directly after a successful peek.
    fn take_peeked(
        &mut self,
        expected: &'static str,
    ) -> Result<SyntaxToken<S::Kind, S::Trivia>, S::Error> {
        match self.peeked.take() {
            Some(Significant::Token(token)) => Ok(token),
            Some(end @ Significant::End { .. }) => {
                let Significant::End { position, .. } = &end else {
                    unreachable!()
                };
                let position = *position;
                self.peeked = Some(end);

                Err(self.syntax.unexpected_eof(position, expected))
            }
            None => {
                let step = self.next_significant()?;
                self.peeked = Some(step);
                self.take_peeked(expected)
            }
        }
    }

    /// Parses one value. `depth` is the number of containers already entered.
    fn parse_value(&mut self, depth: usize) -> Result<Value<S::Kind, S::Trivia>, S::Error> {
        let token = match self.next_significant()? {
            Significant::End { position, .. } => {
                return Err(self
                    .syntax
                    .unexpected_eof(position, self.syntax.value_expectation()));
            }
            Significant::Token(token) => token,
        };

        match self.syntax.value_role(token.kind) {
            Some(ValueRole::ObjectOpen) => self.parse_object(token, depth),
            Some(ValueRole::ArrayOpen) => self.parse_array(token, depth),
            Some(ValueRole::String) => Ok(Value::String(token)),
            Some(ValueRole::Number) => Ok(Value::Number(token)),
            Some(ValueRole::Null) => Ok(Value::Null(token)),
            Some(ValueRole::True) => Ok(Value::Bool { value: true, token }),
            Some(ValueRole::False) => Ok(Value::Bool {
                value: false,
                token,
            }),
            None => Err(self.syntax.expected(
                token.start(),
                self.syntax.value_expectation(),
                token.kind,
            )),
        }
    }

    /// Checks the depth limit on entering a container, returning the child depth.
    fn enter_container(
        &self,
        depth: usize,
        open: &SyntaxToken<S::Kind, S::Trivia>,
    ) -> Result<usize, S::Error> {
        let entered = depth.saturating_add(1);

        if entered > self.max_depth {
            Err(self.syntax.depth_exceeded(open.start(), self.max_depth))
        } else {
            Ok(entered)
        }
    }

    fn parse_object(
        &mut self,
        open: SyntaxToken<S::Kind, S::Trivia>,
        depth: usize,
    ) -> Result<Value<S::Kind, S::Trivia>, S::Error> {
        let depth = self.enter_container(depth, &open)?;
        let mut members = Vec::new();

        if self.peek_punct()? == Some(Punct::ObjectClose) {
            let close = self.take_peeked("'}'")?;
            return Ok(Value::Object(ObjectNode::new(open, members, close)));
        }

        loop {
            let name = match self.next_significant()? {
                Significant::End { position, .. } => {
                    return Err(self
                        .syntax
                        .unexpected_eof(position, self.syntax.member_name_expectation()));
                }
                Significant::Token(token) if self.syntax.is_member_name(token.kind) => token,
                Significant::Token(token) => {
                    return Err(self.syntax.expected(
                        token.start(),
                        self.syntax.member_name_expectation(),
                        token.kind,
                    ));
                }
            };

            let colon = self.expect_punct(Punct::Colon, "':'")?;
            let value = self.parse_value(depth)?;

            match self.next_significant()? {
                Significant::End { position, .. } => {
                    return Err(self.syntax.unexpected_eof(position, "',' or '}'"));
                }
                Significant::Token(separator) => match self.syntax.punct(separator.kind) {
                    Some(Punct::Comma) => {
                        members.push(Member::new(name, colon, value, Some(separator)));

                        if self.syntax.allow_trailing_commas()
                            && self.peek_punct()? == Some(Punct::ObjectClose)
                        {
                            let close = self.take_peeked("'}'")?;
                            return Ok(Value::Object(ObjectNode::new(open, members, close)));
                        }
                    }
                    Some(Punct::ObjectClose) => {
                        members.push(Member::new(name, colon, value, None));
                        return Ok(Value::Object(ObjectNode::new(open, members, separator)));
                    }
                    _ => {
                        return Err(self.syntax.expected(
                            separator.start(),
                            "',' or '}'",
                            separator.kind,
                        ));
                    }
                },
            }
        }
    }

    fn parse_array(
        &mut self,
        open: SyntaxToken<S::Kind, S::Trivia>,
        depth: usize,
    ) -> Result<Value<S::Kind, S::Trivia>, S::Error> {
        let depth = self.enter_container(depth, &open)?;
        let mut elements = Vec::new();

        if self.peek_punct()? == Some(Punct::ArrayClose) {
            let close = self.take_peeked("']'")?;
            return Ok(Value::Array(ArrayNode::new(open, elements, close)));
        }

        loop {
            let value = self.parse_value(depth)?;

            match self.next_significant()? {
                Significant::End { position, .. } => {
                    return Err(self.syntax.unexpected_eof(position, "',' or ']'"));
                }
                Significant::Token(separator) => match self.syntax.punct(separator.kind) {
                    Some(Punct::Comma) => {
                        elements.push(Element::new(value, Some(separator)));

                        if self.syntax.allow_trailing_commas()
                            && self.peek_punct()? == Some(Punct::ArrayClose)
                        {
                            let close = self.take_peeked("']'")?;
                            return Ok(Value::Array(ArrayNode::new(open, elements, close)));
                        }
                    }
                    Some(Punct::ArrayClose) => {
                        elements.push(Element::new(value, None));
                        return Ok(Value::Array(ArrayNode::new(open, elements, separator)));
                    }
                    _ => {
                        return Err(self.syntax.expected(
                            separator.start(),
                            "',' or ']'",
                            separator.kind,
                        ));
                    }
                },
            }
        }
    }

    /// Consumes the next significant token, requiring the given punctuation role.
    fn expect_punct(
        &mut self,
        punct: Punct,
        describe: &'static str,
    ) -> Result<SyntaxToken<S::Kind, S::Trivia>, S::Error> {
        match self.next_significant()? {
            Significant::End { position, .. } => {
                Err(self.syntax.unexpected_eof(position, describe))
            }
            Significant::Token(token) if self.syntax.punct(token.kind) == Some(punct) => Ok(token),
            Significant::Token(token) => {
                Err(self.syntax.expected(token.start(), describe, token.kind))
            }
        }
    }
}
