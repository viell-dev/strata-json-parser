//! The concrete syntax tree (CST): a lossless, zero-copy document shape, generic over the token
//! and trivia kinds of a JSON-family language.
//!
//! The CST retains what the author wrote, not just what it evaluates to: every syntactic token
//! keeps its positions (and derives its byte span from them), trivia is attached to the token it
//! precedes instead of being discarded, and string/number content stays as spans into the
//! retained source — no eager decoding, no owned copies. Concatenating every token's leading
//! trivia and lexeme in source order reproduces the input byte-for-byte.
//!
//! Duplicate object member names are retained verbatim, in source order. RFC 8259 §4 says names
//! *should* be unique but does not require it; a fidelity-preserving CST records what was
//! written and leaves duplicate policy to downstream consumers.
//!
//! The type parameters are `K`, the language's significant token kind, and `T`, its trivia kind.
//! Superset languages reuse these types with their own kinds instead of mirroring the tree —
//! `strata-jsonc-parser` instantiates them as `SyntaxToken<JsonToken, TriviaKind>` with a trivia
//! kind that adds comments. Nodes are only constructed by the grammar driver in this crate
//! ([`crate::grammar::parse_document`]), so a tree with a mismatched token in a structural slot,
//! or positions that disagree with its span, is unrepresentable downstream.

use strata_lexer::{Position, SourceRef, SourceStore, Span, Token};

/// Classification hooks a trivia kind provides so the generic CST can offer kind-aware
/// conveniences.
///
/// Each parser crate's trivia enum implements this; the defaults say "no such trivia exists in
/// this language", so a language without comments or BOM tolerance implements the trait with an
/// empty body.
pub trait TriviaClass: Copy {
    /// Returns whether this trivia piece is a comment.
    #[inline]
    #[must_use]
    fn is_comment(self) -> bool {
        false
    }

    /// Returns whether this trivia piece is a byte-order mark.
    #[inline]
    #[must_use]
    fn is_bom(self) -> bool {
        false
    }
}

/// A retained piece of grammatically insignificant source content.
///
/// Constructed only by the grammar driver, from lexer tokens whose positions are ordered by
/// construction — the span always agrees with the positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trivia<T> {
    kind: T,
    start: Position,
    end: Position,
}

impl<T: Copy> Trivia<T> {
    /// Creates a trivia piece from a classified lexer token.
    pub(crate) fn from_token<L>(kind: T, token: &Token<L>) -> Self {
        Self {
            kind,
            start: token.start(),
            end: token.end(),
        }
    }

    /// What kind of trivia this piece is.
    #[inline]
    #[must_use]
    pub const fn kind(&self) -> T {
        self.kind
    }

    /// Absolute byte range in the source.
    #[inline]
    #[must_use]
    pub const fn span(&self) -> Span {
        Span::from_start_len(
            self.start.byte(),
            self.end.byte().saturating_sub(self.start.byte()),
        )
    }

    /// Position of the first byte.
    #[inline]
    #[must_use]
    pub const fn start(&self) -> Position {
        self.start
    }

    /// Position just past the last byte.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Position {
        self.end
    }

    /// The number of line breaks inside this trivia piece.
    #[must_use]
    pub const fn line_breaks(&self) -> u64 {
        self.end.line().saturating_sub(self.start.line())
    }

    /// The number of blank lines inside this trivia piece.
    ///
    /// A whitespace run separating two tokens contains one line break when the tokens merely sit
    /// on adjacent lines; every additional break is a blank line — the kind of implicit visual
    /// grouping the Strata ecosystem preserves.
    #[must_use]
    pub const fn blank_lines(&self) -> u64 {
        self.line_breaks().saturating_sub(1)
    }
}

/// A significant token in the CST: kind, positions, and the trivia that preceded it.
///
/// The byte span is derived from the positions, so span and positions cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxToken<K, T> {
    /// The token kind.
    pub kind: K,
    start: Position,
    end: Position,
    leading: Vec<Trivia<T>>,
}

impl<K, T> SyntaxToken<K, T> {
    /// Creates a syntax token from a classified lexer token and its leading trivia.
    pub(crate) fn from_token<L>(kind: K, token: &Token<L>, leading: Vec<Trivia<T>>) -> Self {
        Self {
            kind,
            start: token.start(),
            end: token.end(),
            leading,
        }
    }

    /// Absolute byte range of the lexeme (excluding leading trivia).
    #[inline]
    #[must_use]
    pub const fn span(&self) -> Span {
        Span::from_start_len(
            self.start.byte(),
            self.end.byte().saturating_sub(self.start.byte()),
        )
    }

    /// Position of the first lexeme byte.
    #[inline]
    #[must_use]
    pub const fn start(&self) -> Position {
        self.start
    }

    /// Position just past the last lexeme byte.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Position {
        self.end
    }

    /// Trivia between the previous significant token and this one, in source order.
    #[inline]
    #[must_use]
    pub fn leading(&self) -> &[Trivia<T>] {
        &self.leading
    }
}

impl<K, T: TriviaClass> SyntaxToken<K, T> {
    /// The comments among this token's leading trivia, in source order.
    pub fn leading_comments(&self) -> impl Iterator<Item = &Trivia<T>> {
        self.leading
            .iter()
            .filter(|trivia| trivia.kind().is_comment())
    }
}

/// A JSON-family value node.
///
/// Scalar variants carry the token whose span addresses the raw source bytes — a string token's
/// span includes its quotes, and its content is *not* decoded here. Container variants keep
/// every structural token, including the commas between entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<K, T> {
    /// The `null` literal.
    Null(SyntaxToken<K, T>),
    /// A `true` or `false` literal.
    Bool {
        /// The literal's value.
        value: bool,
        /// The literal token.
        token: SyntaxToken<K, T>,
    },
    /// A number; the token span covers the raw, unconverted source text.
    Number(SyntaxToken<K, T>),
    /// A string; the token span covers the raw source text including its quotes.
    String(SyntaxToken<K, T>),
    /// An array.
    Array(ArrayNode<K, T>),
    /// An object.
    Object(ObjectNode<K, T>),
}

impl<K, T> Value<K, T> {
    /// The first significant token of this value, whose leading trivia is everything between the
    /// previous token and this value.
    pub const fn first_token(&self) -> &SyntaxToken<K, T> {
        match self {
            Self::Null(token)
            | Self::Number(token)
            | Self::String(token)
            | Self::Bool { token, .. } => token,
            Self::Array(array) => &array.open,
            Self::Object(object) => &object.open,
        }
    }

    /// The last significant token of this value.
    pub const fn last_token(&self) -> &SyntaxToken<K, T> {
        match self {
            Self::Null(token)
            | Self::Number(token)
            | Self::String(token)
            | Self::Bool { token, .. } => token,
            Self::Array(array) => &array.close,
            Self::Object(object) => &object.close,
        }
    }

    /// The absolute byte range this value covers, from its first to its last lexeme byte
    /// (leading trivia excluded).
    pub const fn span(&self) -> Span {
        let start = self.first_token().span().start();
        let end = self.last_token().span().end();

        Span::from_start_len(start, end.saturating_sub(start))
    }

    /// Visits every significant token of this value in source order.
    pub fn for_each_token<'value>(&'value self, visit: &mut impl FnMut(&'value SyntaxToken<K, T>)) {
        match self {
            Self::Null(token)
            | Self::Number(token)
            | Self::String(token)
            | Self::Bool { token, .. } => visit(token),
            Self::Array(array) => {
                visit(&array.open);

                for element in &array.elements {
                    element.value.for_each_token(visit);

                    if let Some(comma) = &element.comma {
                        visit(comma);
                    }
                }

                visit(&array.close);
            }
            Self::Object(object) => {
                visit(&object.open);

                for member in &object.members {
                    visit(&member.name);
                    visit(&member.colon);
                    member.value.for_each_token(visit);

                    if let Some(comma) = &member.comma {
                        visit(comma);
                    }
                }

                visit(&object.close);
            }
        }
    }
}

/// An array node: `[` *elements* `]`.
///
/// Constructed only by the grammar driver, so the open and close slots always hold the language's
/// actual bracket tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayNode<K, T> {
    open: SyntaxToken<K, T>,
    elements: Vec<Element<K, T>>,
    close: SyntaxToken<K, T>,
}

impl<K, T> ArrayNode<K, T> {
    pub(crate) const fn new(
        open: SyntaxToken<K, T>,
        elements: Vec<Element<K, T>>,
        close: SyntaxToken<K, T>,
    ) -> Self {
        Self {
            open,
            elements,
            close,
        }
    }

    /// The `[` token.
    #[inline]
    #[must_use]
    pub const fn open(&self) -> &SyntaxToken<K, T> {
        &self.open
    }

    /// The elements in source order.
    #[inline]
    #[must_use]
    pub fn elements(&self) -> &[Element<K, T>] {
        &self.elements
    }

    /// The `]` token.
    #[inline]
    #[must_use]
    pub const fn close(&self) -> &SyntaxToken<K, T> {
        &self.close
    }
}

/// One array element and the comma that followed it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element<K, T> {
    value: Value<K, T>,
    comma: Option<SyntaxToken<K, T>>,
}

impl<K, T> Element<K, T> {
    pub(crate) const fn new(value: Value<K, T>, comma: Option<SyntaxToken<K, T>>) -> Self {
        Self { value, comma }
    }

    /// The element value.
    #[inline]
    #[must_use]
    pub const fn value(&self) -> &Value<K, T> {
        &self.value
    }

    /// The `,` after this element; `None` on the last element without a trailing comma.
    #[inline]
    #[must_use]
    pub const fn comma(&self) -> Option<&SyntaxToken<K, T>> {
        self.comma.as_ref()
    }
}

/// An object node: `{` *members* `}`.
///
/// Constructed only by the grammar driver, so the open and close slots always hold the language's
/// actual brace tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectNode<K, T> {
    open: SyntaxToken<K, T>,
    members: Vec<Member<K, T>>,
    close: SyntaxToken<K, T>,
}

impl<K, T> ObjectNode<K, T> {
    pub(crate) const fn new(
        open: SyntaxToken<K, T>,
        members: Vec<Member<K, T>>,
        close: SyntaxToken<K, T>,
    ) -> Self {
        Self {
            open,
            members,
            close,
        }
    }

    /// The `{` token.
    #[inline]
    #[must_use]
    pub const fn open(&self) -> &SyntaxToken<K, T> {
        &self.open
    }

    /// The members in source order, duplicates included.
    #[inline]
    #[must_use]
    pub fn members(&self) -> &[Member<K, T>] {
        &self.members
    }

    /// The `}` token.
    #[inline]
    #[must_use]
    pub const fn close(&self) -> &SyntaxToken<K, T> {
        &self.close
    }
}

/// One object member: *name* `:` *value*, plus the comma that followed it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member<K, T> {
    name: SyntaxToken<K, T>,
    colon: SyntaxToken<K, T>,
    value: Value<K, T>,
    comma: Option<SyntaxToken<K, T>>,
}

impl<K, T> Member<K, T> {
    pub(crate) const fn new(
        name: SyntaxToken<K, T>,
        colon: SyntaxToken<K, T>,
        value: Value<K, T>,
        comma: Option<SyntaxToken<K, T>>,
    ) -> Self {
        Self {
            name,
            colon,
            value,
            comma,
        }
    }

    /// The member name token; for JSON a string whose span includes both quotes.
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &SyntaxToken<K, T> {
        &self.name
    }

    /// The `:` token.
    #[inline]
    #[must_use]
    pub const fn colon(&self) -> &SyntaxToken<K, T> {
        &self.colon
    }

    /// The member value.
    #[inline]
    #[must_use]
    pub const fn value(&self) -> &Value<K, T> {
        &self.value
    }

    /// The `,` after this member; `None` on the last member without a trailing comma.
    #[inline]
    #[must_use]
    pub const fn comma(&self) -> Option<&SyntaxToken<K, T>> {
        self.comma.as_ref()
    }
}

/// A parsed document: the root value, trailing trivia, and the retained source.
///
/// The document owns the [`SourceStore`] holding the original bytes, so every span in the tree
/// stays addressable — zero-copy — for as long as the document lives. [`Document::source_ref`]
/// hands out promotable handles that survive even store reclamation (see `strata-lexer`'s
/// retention layer); [`Document::bytes`] is the plain borrow path. Resolver layers take the tree
/// and the store together via [`Document::into_parts`].
#[derive(Debug)]
pub struct Document<K, T> {
    root: Value<K, T>,
    trailing: Vec<Trivia<T>>,
    source: SourceStore,
}

impl<K, T> Document<K, T> {
    /// Assembles a document from its parts. Used by the grammar driver.
    pub(crate) const fn new(
        root: Value<K, T>,
        trailing: Vec<Trivia<T>>,
        source: SourceStore,
    ) -> Self {
        Self {
            root,
            trailing,
            source,
        }
    }

    /// The root value. Any RFC 8259 value is a valid top-level document (§2).
    pub const fn root(&self) -> &Value<K, T> {
        &self.root
    }

    /// Trivia between the end of the root value and the end of input.
    pub fn trailing_trivia(&self) -> &[Trivia<T>] {
        &self.trailing
    }

    /// Borrows the raw source bytes covered by `span`, zero-copy.
    ///
    /// Returns `None` if the span is not (or no longer) backed by the retained source, e.g.
    /// after [`Document::source_mut`] was used to retire buffers.
    pub fn bytes(&self, span: Span) -> Option<&[u8]> {
        self.source.bytes(span)
    }

    /// Borrows the raw lexeme bytes of `token`, zero-copy.
    pub fn token_bytes(&self, token: &SyntaxToken<K, T>) -> Option<&[u8]> {
        self.bytes(token.span())
    }

    /// Creates a promotable, owned-on-demand handle to the bytes covered by `span`.
    ///
    /// This is the resolver-facing path: the handle stays zero-copy while the store retains the
    /// buffer and degrades to a small owned copy if the store reclaims it.
    pub fn source_ref(&mut self, span: Span) -> Option<SourceRef> {
        self.source.source_ref(span)
    }

    /// The retained source store.
    pub const fn source(&self) -> &SourceStore {
        &self.source
    }

    /// Mutable access to the retained source store, e.g. for owner-side reclamation
    /// (`maintain` / `retire_all`).
    pub const fn source_mut(&mut self) -> &mut SourceStore {
        &mut self.source
    }

    /// Decomposes the document into the tree, its trailing trivia, and the retained source.
    ///
    /// This is the resolver boundary: a resolver takes ownership of the tree and the store,
    /// decodes what it needs, and decides how long the retained source lives.
    #[must_use]
    pub fn into_parts(self) -> (Value<K, T>, Vec<Trivia<T>>, SourceStore) {
        (self.root, self.trailing, self.source)
    }
}

impl<K, T: TriviaClass> Document<K, T> {
    /// The bytes of trivia `piece`, zero-copy.
    pub fn trivia_bytes(&self, piece: &Trivia<T>) -> Option<&[u8]> {
        self.bytes(piece.span())
    }

    /// The tolerated leading byte-order mark, if the input carried one and the parser was
    /// configured to tolerate it.
    pub fn bom(&self) -> Option<&Trivia<T>> {
        self.root
            .first_token()
            .leading()
            .first()
            .filter(|trivia| trivia.kind().is_bom())
    }
}
