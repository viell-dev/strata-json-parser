# strata-json-parser

`strata-json-parser` is the strict RFC 8259 JSON parser for the Strata parser ecosystem. It turns
a byte stream into a lossless, zero-copy concrete syntax tree (CST): tokens keep absolute byte
spans, trivia (whitespace runs, blank lines, a tolerated byte-order mark) is retained instead of
discarded, and string/number content stays as raw source bytes. Decoding lives in this crate's
`decode` module but runs at the resolver layer, not during parsing.

The crate sits at the parser stage of the per-language stack, and — uniquely in the JSON family —
hosts the *generic* CST and grammar driver every superset parser instantiates:

```text
strata-json-rules -> strata-json-parser (this crate) -> strata-json (resolver)
                         ^ generic cst + grammar reused by
                           strata-jsonc-parser and strata-json5-parser
```

- **Edition / MSRV:** Rust 2024, MSRV `1.87.0`.
- **License:** MPL-2.0.
- **Dependencies:** `strata-json-rules`, `strata-lexer`.

## 1. Module and public-API map

| Module    | Public items |
|-----------|--------------|
| `cst`     | `Document<K, T>`, `Value<K, T>`, `ArrayNode`, `Element`, `ObjectNode`, `Member`, `SyntaxToken<K, T>`, `Trivia<T>`, `TriviaClass` |
| `grammar` | `Syntax` (trait), `parse_document()`, `Lexed<K, T>`, `ValueRole`, `Punct` |
| `decode`  | `unescape_string()`, `parse_number()`, `Number`, `LoneSurrogates`, `Utf16Decoder`, `Utf16Step`, `LoneSurrogate`, `Error`, `ErrorKind` |
| root      | `JsonParser<M>`, `JsonParserBuilder<M>`, `JsonError`, `JsonErrorKind`, `DEFAULT_MAX_DEPTH`, `TriviaKind`, non-generic aliases (`Document`, `Value`, `SyntaxToken`, `Trivia`, ...), and the `strata-json-rules` surface re-exported |

## 2. The generic CST (`cst`)

The tree is parameterized over `K` (the language's significant token kind) and `T` (its trivia
kind), so superset parsers instantiate it instead of mirroring it: `strata-jsonc-parser` uses
`SyntaxToken<JsonToken, TriviaKind>` with a trivia kind that adds comments;
`strata-json5-parser` uses `SyntaxToken<Json5Token, ...>`.

- The tree is lossless: concatenating every token's leading trivia and lexeme in source order
  reproduces the input byte-for-byte (`tests/trivia_spans.rs` proves this).
- `SyntaxToken<K, T>` — public `kind`, private ordered positions with `span()` *derived* from
  them (a token whose span disagrees with its positions is unrepresentable), and `leading()`
  trivia. `Trivia<T>` mirrors the shape; `line_breaks()`/`blank_lines()` surface the implicit
  visual grouping the ecosystem preserves.
- Nodes (`ArrayNode`, `ObjectNode`, `Element`, `Member`) have private fields with read accessors
  and crate-private constructors: only the grammar driver builds them, so a tree with a
  mismatched token in a structural slot cannot exist downstream. `Value` stays a public enum for
  pattern matching; its scalar payloads are driver-built tokens.
- `TriviaClass` is the small classification hook (`is_comment()`, `is_bom()`) each language's
  trivia enum implements; it powers the kind-aware conveniences (`Document::bom()`,
  `SyntaxToken::leading_comments()`, `Document::trivia_bytes()`).
- `Document<K, T>` owns the root, trailing trivia, and the `SourceStore` holding the parked
  source bytes. Access: `bytes(span)` / `token_bytes(&token)` (zero-copy borrows),
  `source_ref(span)` (promotable handle that survives store reclamation), `source()` /
  `source_mut()`, and `into_parts()` — the resolver boundary, handing over tree, trailing
  trivia, and store.
- Duplicate member names are retained verbatim in source order (RFC 8259 §4 SHOULD, not MUST);
  duplicate policy belongs to resolvers.

## 3. The grammar driver (`grammar`)

The `value (object | array | scalar)` grammar with comma-separated entries is identical across
the JSON family; what differs is captured by the `Syntax` trait: token classification
(`classify`, where per-token policy like BOM rejection lives), which kinds open values
(`value_role`) or name members (`is_member_name`), structural punctuation (`punct`), diagnostic
wording, `allow_trailing_commas`, and the concrete error type (built through error-constructor
methods so every language keeps its own precise enum).

`parse_document(reader, rules, &syntax, max_depth, lexer_config)` is the one recursive-descent
driver written against the trait. It owns trivia folding (every trivia piece attaches to the
next significant token, or to the document end), the nesting-depth limit, the whole-input
requirement, and parking the consumed bytes into the document's `SourceStore` (compaction is
forced off during the parse so the bytes survive). Trailing-comma tolerance is implemented once
in the driver, gated by the trait.

This replaces the first-generation POCs' per-crate mirrors: the JSONC and JSON5 parsers are now
a `Syntax` impl plus type aliases each, instead of ~400 duplicated CST lines and a duplicated
driver. No `strata-lexer` changes were needed — the lexer was already kind-generic.

## 4. The JSON front (root)

`JsonParser::new()` / `default()` is the fully strict path: strict rules, no BOM tolerance,
`DEFAULT_MAX_DEPTH` (128) nesting. `parse(impl Read)`, `parse_str`, and `parse_bytes` each run
one document to EOF. The parser clones its rule set into a fresh scanner per parse, so one
parser serves many inputs.

`JsonParser::builder()` exposes, per the ecosystem's builders-per-layer shape: `with_rules(set)`
(any `MatcherSet<Kind = JsonToken> + Clone`; wrap non-`Clone` sets in `SharedRuleSet`),
`with_lexer_config(config)`, `allow_bom(bool)` (§8.1 permission, retained as `TriviaKind::Bom`
when on), and `max_depth(usize)` (§9 implementation limit, also bounds recursion).

Errors are `JsonError { position, kind }` with one-based line/column plus absolute byte.
`JsonErrorKind`: `Lex(LexError)`, `Expected`, `UnexpectedEof`, `TrailingContent`,
`UnexpectedBom`, `DepthLimitExceeded`.

## 5. Decoding (`decode`)

The parser never decodes; the resolver layer does, through this module — placed here because the
parser layer owns the inverse of the shapes its rules validate, and because resolvers must not
depend on each other (the JSONC resolver reuses JSON decoding through its parser's re-export).

- `unescape_string(raw, policy)` — both quotes included; handles the §7 escapes and combines
  UTF-16 surrogate pairs via the streaming `Utf16Decoder` (reused by JSON5's decode for its
  shared escapes). Lone surrogates resolve per `LoneSurrogates` (`Error`, the strict default, or
  `Replace` with U+FFFD). Errors carry the byte offset of the offending escape's backslash.
- `parse_number(raw)` — integer-preserving classification: `Number::U64` for non-negative
  integers that fit, `I64` for negative, `F64` otherwise (fractions, exponents, `-0` keeping its
  sign, and 64-bit overflow with documented rounding; magnitudes beyond `f64` become infinite).
  Raw text stays reachable through the CST for arbitrary-precision consumers.
- The functions are total over arbitrary input: parser-produced tokens cannot be malformed, but
  the functions are public and re-check shape (`ErrorKind::MalformedToken`) instead of trusting
  callers.

## 6. Design choices

- **Construction-time validation.** CST nodes are only constructible by the driver; token
  spans are derived from ordered positions; the lexer level already makes reversed spans and
  zero-length matches unrepresentable. Downstream layers never re-check these.
- **Generic tree over mirrored trees.** The `(K, T)` parameterization plus `Syntax` moves all
  language variance into one trait impl per language. The alternative (per-language copies)
  was proven out and rejected in the first-generation POCs.
- **Structure/data separation.** No `String` payloads in tokens or nodes; spans plus the
  document-owned `SourceStore` keep the tree zero-copy until a caller resolves bytes.
- **Leading-trivia attachment.** Every trivia piece precedes exactly one significant token (or
  EOF), which makes reconstruction trivial and keeps trivia queries local.

## 7. Quality bar

Same as the ecosystem: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings` under the strict lint set in `Cargo.toml`, and `cargo test` all clean. Lint exceptions
use `#[expect(lint, reason = "…")]` on the smallest existing scope.

## 8. Limitations and follow-up

- Single-buffer retention: the parser parks one consumed buffer after EOF; streaming/chunked DOM
  construction awaits the lexer's batch protocol.
- Serde glue (parser → structs directly) is a separate planned crate, not part of this one.
- The public API is not settled; this is an initial-development crate on an experimental branch.
- Package verification (`cargo package`) is blocked until the path dependencies are published.
