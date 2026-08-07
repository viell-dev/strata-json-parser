//! Strict RFC 8259 JSON parser for the Strata parser ecosystem.
//!
//! The parser turns `strata-json-rules` tokens into a lossless, zero-copy CST: tokens keep
//! their absolute byte spans, trivia (whitespace runs, blank lines, a tolerated BOM) is retained
//! and attached to the token it precedes, and string/number content stays raw — decoding is the
//! resolver layer's job, served by this crate's [`decode`] module.
//!
//! The crate has three consumer-facing layers:
//!
//! - The root: [`JsonParser`] / [`JsonParserBuilder`] parse JSON into a [`Document`], with the
//!   two RFC-permitted options ([`allow_bom`](JsonParserBuilder::allow_bom) §8.1,
//!   [`max_depth`](JsonParserBuilder::max_depth) §9) and hooks for swapping lower layers.
//! - [`cst`] and [`grammar`]: the generic tree and grammar driver, parameterized over token and
//!   trivia kinds. Superset languages (JSONC, JSON5, ...) implement [`grammar::Syntax`] and
//!   instantiate the same tree with their own kinds instead of mirroring either.
//! - [`decode`]: string unescaping (with surrogate-pair combination and a lone-surrogate
//!   policy) and integer-preserving number conversion, shared by the resolver crates.

pub mod cst;
pub mod decode;
pub mod grammar;
mod parse;

pub use parse::{
    ArrayNode, DEFAULT_MAX_DEPTH, Document, Element, JsonError, JsonErrorKind, JsonParser,
    JsonParserBuilder, Member, ObjectNode, SyntaxToken, Trivia, TriviaKind, Value,
};
pub use strata_json_rules::{
    BomRule, JsonToken, LiteralRule, NumberRule, PunctRule, SharedRuleSet, StrictRuleSet,
    StringRule, WhitespaceRule, strict_rule_set, strict_scanner,
};
