//! The RFC-permitted parser options: BOM tolerance (§8.1) and the nesting-depth limit (§9),
//! plus the builder's layer-replacement hooks.

use strata_json_parser::{
    DEFAULT_MAX_DEPTH, Document, JsonError, JsonErrorKind, JsonParser, Trivia, TriviaKind, Value,
};
use strata_json_rules::{SharedRuleSet, strict_rule_set};
use strata_lexer::{LexErrorKind, LexerConfig};

fn nested_arrays(depth: usize) -> String {
    let mut out = String::new();

    for _ in 0..depth {
        out.push('[');
    }

    out.push('1');

    for _ in 0..depth {
        out.push(']');
    }

    out
}

// ── BOM (§8.1) ───────────────────────────────────────────────────────────────

#[test]
fn strict_parser_rejects_a_leading_bom() {
    let Err(error) = JsonParser::new().parse_str("\u{FEFF}{}") else {
        panic!("expected the BOM to be rejected");
    };
    assert!(matches!(error.kind, JsonErrorKind::UnexpectedBom));
    assert_eq!(error.position.byte(), 0);
}

#[test]
fn allow_bom_tolerates_a_leading_bom_as_trivia() -> Result<(), JsonError> {
    let parser = JsonParser::builder().allow_bom(true).build();
    let document = parser.parse_str("\u{FEFF}{\"a\": 1}")?;

    let Some(bom) = document.bom() else {
        panic!("expected the BOM to be retained");
    };
    assert_eq!(bom.kind(), TriviaKind::Bom);
    assert_eq!(bom.span().start(), 0);
    assert_eq!(bom.span().end(), 3);

    // The value tokens sit after the BOM.
    assert_eq!(document.root().first_token().span().start(), 3);

    Ok(())
}

#[test]
fn allow_bom_retains_bom_followed_by_whitespace() -> Result<(), JsonError> {
    let parser = JsonParser::builder().allow_bom(true).build();
    let document = parser.parse_str("\u{FEFF}  1")?;

    let leading = document.root().first_token().leading();
    assert_eq!(leading.len(), 2);
    assert!(matches!(
        leading.first().map(Trivia::kind),
        Some(TriviaKind::Bom)
    ));
    assert!(matches!(
        leading.last().map(Trivia::kind),
        Some(TriviaKind::Whitespace)
    ));

    Ok(())
}

#[test]
fn documents_without_a_bom_report_none() -> Result<(), JsonError> {
    let parser = JsonParser::builder().allow_bom(true).build();
    assert!(parser.parse_str("1")?.bom().is_none());
    assert!(parser.parse_str("  1")?.bom().is_none());
    Ok(())
}

#[test]
fn bom_after_the_first_byte_is_always_an_error() {
    for parser in [
        JsonParser::new(),
        JsonParser::builder().allow_bom(true).build(),
    ] {
        let Err(error) = parser.parse_str("1 \u{FEFF}") else {
            panic!("expected a mid-input BOM to fail");
        };
        let JsonErrorKind::Lex(lex) = &error.kind else {
            panic!("expected a lexical error, got {error:?}");
        };
        assert!(matches!(lex.kind, LexErrorKind::Unrecognized));
        assert_eq!(error.position.byte(), 2);
    }
}

#[test]
fn bom_only_input_is_still_missing_a_value() {
    let parser = JsonParser::builder().allow_bom(true).build();
    let Err(error) = parser.parse_str("\u{FEFF}") else {
        panic!("expected a BOM-only input to fail");
    };
    assert!(matches!(error.kind, JsonErrorKind::UnexpectedEof { .. }));
}

// ── nesting depth (§9) ───────────────────────────────────────────────────────

#[test]
fn default_depth_limit_is_documented_and_enforced() {
    assert_eq!(JsonParser::new().max_depth(), DEFAULT_MAX_DEPTH);

    let at_limit = nested_arrays(DEFAULT_MAX_DEPTH);
    assert!(JsonParser::new().parse_str(&at_limit).is_ok());

    let beyond = nested_arrays(DEFAULT_MAX_DEPTH.saturating_add(1));
    let Err(error) = JsonParser::new().parse_str(&beyond) else {
        panic!("expected the depth limit to trip");
    };
    assert!(matches!(
        error.kind,
        JsonErrorKind::DepthLimitExceeded {
            limit: DEFAULT_MAX_DEPTH,
        }
    ));
    // The error points at the first open bracket beyond the limit.
    assert_eq!(
        error.position.byte(),
        u64::try_from(DEFAULT_MAX_DEPTH).unwrap_or(u64::MAX)
    );
}

#[test]
fn custom_depth_limit_counts_objects_and_arrays() {
    let parser = JsonParser::builder().max_depth(2).build();

    assert!(parser.parse_str("[[1]]").is_ok());
    assert!(parser.parse_str(r#"{"a": [1]}"#).is_ok());

    let Err(error) = parser.parse_str("[[[1]]]") else {
        panic!("expected depth 3 to fail with limit 2");
    };
    assert!(matches!(
        error.kind,
        JsonErrorKind::DepthLimitExceeded { limit: 2 }
    ));

    let Err(error) = parser.parse_str(r#"{"a": {"b": {"c": 1}}}"#) else {
        panic!("expected object depth 3 to fail with limit 2");
    };
    assert!(matches!(
        error.kind,
        JsonErrorKind::DepthLimitExceeded { limit: 2 }
    ));
}

#[test]
fn zero_depth_limit_still_parses_scalars() {
    let parser = JsonParser::builder().max_depth(0).build();

    assert!(parser.parse_str("1").is_ok());
    assert!(parser.parse_str("\"s\"").is_ok());

    let Err(error) = parser.parse_str("[]") else {
        panic!("expected any container to fail with limit 0");
    };
    assert!(matches!(
        error.kind,
        JsonErrorKind::DepthLimitExceeded { limit: 0 }
    ));
}

// ── layer replacement ────────────────────────────────────────────────────────

#[test]
fn with_lexer_config_is_honored() -> Result<(), JsonError> {
    // A tiny initial reader buffer still parses: the reader grows on demand, and the parser
    // keeps compaction off so retained spans stay addressable.
    let config = LexerConfig::new().with_reader_initial_capacity(8);
    let parser = JsonParser::builder().with_lexer_config(config).build();

    let input = r#"{"key": "a longer value than the initial buffer", "n": [1, 2, 3]}"#;
    let document: Document = parser.parse_str(input)?;

    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let Some(member) = object.members().first() else {
        panic!("expected members");
    };
    assert_eq!(
        document.bytes(member.value().span()),
        Some(&b"\"a longer value than the initial buffer\""[..])
    );

    Ok(())
}

#[test]
fn with_rules_accepts_a_shared_rule_set() -> Result<(), JsonError> {
    // SharedRuleSet makes a non-Copy rule set cloneable, so one parser can be reused.
    let parser = JsonParser::builder()
        .with_rules(SharedRuleSet::new(strict_rule_set()))
        .build();

    assert!(matches!(parser.parse_str("[1]")?.root(), Value::Array(_)));
    assert!(matches!(parser.parse_str("{}")?.root(), Value::Object(_)));

    Ok(())
}
