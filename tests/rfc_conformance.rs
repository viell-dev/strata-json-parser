//! RFC 8259 conformance: the strict default parser accepts exactly the RFC grammar.

use strata_json_parser::{
    Document, Element, JsonError, JsonErrorKind, JsonParser, JsonToken, Value,
};
use strata_lexer::LexErrorKind;

fn parse(input: &str) -> Result<Document, JsonError> {
    JsonParser::new().parse_str(input)
}

fn parse_err(input: &str) -> JsonError {
    match parse(input) {
        Err(error) => error,
        Ok(document) => panic!("expected {input:?} to fail, parsed {document:?}"),
    }
}

fn assert_lex_err(input: &str, kind: &LexErrorKind, byte: u64) {
    let error = parse_err(input);
    let JsonErrorKind::Lex(lex) = &error.kind else {
        panic!("expected a lexical error for {input:?}, got {error:?}");
    };
    assert_eq!(
        std::mem::discriminant(&lex.kind),
        std::mem::discriminant(kind),
        "input {input:?}, got {lex:?}"
    );
    assert_eq!(error.position.byte(), byte, "input {input:?}");
}

/// The raw source text of the root scalar token.
fn root_text(input: &str) -> String {
    let document = match parse(input) {
        Ok(document) => document,
        Err(error) => panic!("expected {input:?} to parse: {error}"),
    };
    let token = document.root().first_token();
    let Some(bytes) = document.token_bytes(token) else {
        panic!("root token bytes must resolve for {input:?}");
    };
    String::from_utf8_lossy(bytes).into_owned()
}

// ── valid documents: literals ────────────────────────────────────────────────

#[test]
fn null_literal_parses() -> Result<(), JsonError> {
    let document = parse("null")?;
    assert!(matches!(document.root(), Value::Null(_)));
    Ok(())
}

#[test]
fn bool_literals_parse() -> Result<(), JsonError> {
    assert!(matches!(
        parse("true")?.root(),
        Value::Bool { value: true, .. }
    ));
    assert!(matches!(
        parse("false")?.root(),
        Value::Bool { value: false, .. }
    ));
    Ok(())
}

#[test]
fn any_value_is_a_valid_top_level_document() -> Result<(), JsonError> {
    // RFC 8259 §2: a JSON text is a serialized value of any type.
    assert!(matches!(parse("null")?.root(), Value::Null(_)));
    assert!(matches!(parse("0")?.root(), Value::Number(_)));
    assert!(matches!(parse(r#""s""#)?.root(), Value::String(_)));
    assert!(matches!(parse("[]")?.root(), Value::Array(_)));
    assert!(matches!(parse("{}")?.root(), Value::Object(_)));
    Ok(())
}

// ── valid documents: numbers (§6) ────────────────────────────────────────────

#[test]
fn number_source_text_is_preserved_verbatim() {
    for input in [
        "0",
        "-0",
        "42",
        "-17",
        "3.14",
        "-0.5",
        "1e10",
        "2.5E+3",
        "5e-4",
        "1e+5",
        "1E10",
        "9007199254740992",
        "-92233720368547758080000",
        "0.00001",
        "0e0",
    ] {
        assert_eq!(root_text(input), input, "input {input:?}");
    }
}

// ── valid documents: strings (§7) ────────────────────────────────────────────

#[test]
fn string_shapes_are_accepted_verbatim() {
    for input in [
        r#""""#,
        r#""hello world""#,
        r#""\"\\\/\b\f\n\r\t""#, // every single-character escape
        r#""\u20AC""#,           // BMP unicode escape
        r#""\uD83D\uDE00""#,     // surrogate pair escape
        r#""café""#,             // literal multi-byte UTF-8
        "\"\u{1F600}\"",         // literal emoji
        "\"\u{7F}\"",            // DEL is not a control char per §7 (only U+0000..U+001F)
        r#""\uD800""#,           // lone high surrogate: §8.2 discourages, does not forbid
        r#""\uDC00""#,           // lone low surrogate
    ] {
        assert_eq!(root_text(input), input, "input {input:?}");
    }
}

#[test]
fn string_span_covers_both_quotes() -> Result<(), JsonError> {
    let document = parse(r#"  "hi"  "#)?;
    let Value::String(token) = document.root() else {
        panic!("expected a string root");
    };
    assert_eq!(token.span().start(), 2);
    assert_eq!(token.span().end(), 6);
    Ok(())
}

// ── valid documents: structures ──────────────────────────────────────────────

#[test]
fn empty_containers_parse() -> Result<(), JsonError> {
    let document = parse("{}")?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    assert!(object.members().is_empty());

    let document = parse("[]")?;
    let Value::Array(array) = document.root() else {
        panic!("expected an array");
    };
    assert!(array.elements().is_empty());
    Ok(())
}

#[test]
fn object_members_keep_source_order() -> Result<(), JsonError> {
    let document = parse(r#"{"b":1,"a":2,"c":3}"#)?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let names: Vec<_> = object
        .members()
        .iter()
        .filter_map(|member| document.token_bytes(member.name()))
        .collect();
    assert_eq!(names, [&b"\"b\""[..], b"\"a\"", b"\"c\""]);
    Ok(())
}

#[test]
fn duplicate_member_names_are_retained_verbatim() -> Result<(), JsonError> {
    // RFC 8259 §4: names SHOULD be unique; the CST records what was written.
    let document = parse(r#"{"k":1,"k":2}"#)?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    assert_eq!(object.members().len(), 2);

    let values: Vec<_> = object
        .members()
        .iter()
        .filter_map(|member| document.bytes(member.value().span()).map(<[u8]>::to_vec))
        .collect();
    assert_eq!(values, [b"1".to_vec(), b"2".to_vec()]);
    Ok(())
}

#[test]
fn nested_structures_parse() -> Result<(), JsonError> {
    let document = parse(r#"{"items":[null,true,{"x":0}],"empty":{}}"#)?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let Some(first) = object.members().first() else {
        panic!("expected members");
    };
    let Value::Array(items) = first.value() else {
        panic!("expected an array value");
    };
    assert_eq!(items.elements().len(), 3);
    assert!(matches!(
        items.elements().first().map(Element::value),
        Some(Value::Null(_))
    ));
    assert!(matches!(
        items.elements().last().map(Element::value),
        Some(Value::Object(_))
    ));
    Ok(())
}

#[test]
fn commas_are_retained_between_entries() -> Result<(), JsonError> {
    let document = parse("[1, 2, 3]")?;
    let Value::Array(array) = document.root() else {
        panic!("expected an array");
    };
    let commas: Vec<bool> = array
        .elements()
        .iter()
        .map(|element| element.comma().is_some())
        .collect();
    assert_eq!(commas, [true, true, false]);
    Ok(())
}

#[test]
fn insignificant_whitespace_is_allowed_everywhere() -> Result<(), JsonError> {
    let document = parse(" \t\r\n{ \"a\" : [ 1 , true ] , \"b\" : null } \r\n")?;
    assert!(matches!(document.root(), Value::Object(_)));
    Ok(())
}

// ── invalid documents: numbers (§6) ──────────────────────────────────────────

#[test]
fn leading_zeros_are_rejected() {
    assert_lex_err("01", &LexErrorKind::Unrecognized, 1);
    assert_lex_err("-01", &LexErrorKind::Unrecognized, 2);
    assert_lex_err("00", &LexErrorKind::Unrecognized, 1);
    assert_lex_err("0123", &LexErrorKind::Unrecognized, 1);
}

#[test]
fn incomplete_number_parts_are_rejected() {
    // The position points into the incomplete part: at the bare `.`, or just past an
    // exponent marker/sign whose digits are missing.
    assert_lex_err("1.", &LexErrorKind::Unrecognized, 1);
    assert_lex_err("1e", &LexErrorKind::Unrecognized, 2);
    assert_lex_err("1E", &LexErrorKind::Unrecognized, 2);
    assert_lex_err("1e+", &LexErrorKind::Unrecognized, 3);
    assert_lex_err("1e-", &LexErrorKind::Unrecognized, 3);
}

#[test]
fn misshapen_numbers_are_rejected() {
    // No leading plus, no bare dot, no bare minus, no hex, no special values.
    assert_lex_err("+1", &LexErrorKind::Unrecognized, 0);
    assert_lex_err(".1", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("-", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("NaN", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("Infinity", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("-Infinity", &LexErrorKind::Unrecognized, 0);
}

#[test]
fn hex_suffix_after_number_is_trailing_garbage() {
    // `0x1` lexes `0` as a number, then no rule matches `x`.
    assert_lex_err("0x1", &LexErrorKind::Unrecognized, 1);
}

// ── invalid documents: strings (§7) ──────────────────────────────────────────

#[test]
fn invalid_escapes_are_rejected_at_the_backslash() {
    assert_lex_err(r#""\q""#, &LexErrorKind::Unrecognized, 1);
    assert_lex_err(r#""\x41""#, &LexErrorKind::Unrecognized, 1);
    assert_lex_err(r#""ab\'""#, &LexErrorKind::Unrecognized, 3);
    // Capital U is not a unicode escape.
    assert_lex_err(r#""\U0041""#, &LexErrorKind::Unrecognized, 1);
}

#[test]
fn malformed_unicode_escapes_are_rejected() {
    assert_lex_err(r#""\u123""#, &LexErrorKind::Unrecognized, 1);
    assert_lex_err(r#""\u12G4""#, &LexErrorKind::Unrecognized, 1);
    assert_lex_err(r#""\u""#, &LexErrorKind::Unrecognized, 1);
}

#[test]
fn unescaped_control_characters_are_rejected() {
    assert_lex_err("\"\t\"", &LexErrorKind::Unrecognized, 1);
    assert_lex_err("\"a\nb\"", &LexErrorKind::Unrecognized, 2);
    assert_lex_err("\"\u{0}\"", &LexErrorKind::Unrecognized, 1);
    assert_lex_err("\"\u{1F}\"", &LexErrorKind::Unrecognized, 1);
}

#[test]
fn unterminated_strings_are_rejected() {
    assert_lex_err(r#""open"#, &LexErrorKind::UnterminatedDelimiter, 0);
    assert_lex_err(
        r#""ends with escape\"#,
        &LexErrorKind::UnterminatedDelimiter,
        0,
    );
    assert_lex_err(r#""\""#, &LexErrorKind::UnterminatedDelimiter, 0);
}

#[test]
fn invalid_utf8_in_a_string_is_rejected() {
    let result = JsonParser::new().parse_bytes(b"\"a\xFF\"");
    let Err(error) = result else {
        panic!("expected invalid UTF-8 to fail");
    };
    assert!(matches!(
        &error.kind,
        JsonErrorKind::Lex(lex) if matches!(lex.kind, LexErrorKind::Unrecognized)
    ));
    assert_eq!(error.position.byte(), 2);
}

#[test]
fn single_quoted_strings_are_rejected() {
    assert_lex_err("'single'", &LexErrorKind::Unrecognized, 0);
}

// ── invalid documents: literals (§3) ─────────────────────────────────────────

#[test]
fn misspelled_literals_are_rejected() {
    assert_lex_err("True", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("FALSE", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("NULL", &LexErrorKind::Unrecognized, 0);
    assert_lex_err("nul", &LexErrorKind::Unrecognized, 0);
    // `nulll` lexes `null`, then no rule matches the trailing `l`.
    assert_lex_err("nulll", &LexErrorKind::Unrecognized, 4);
}

// ── invalid documents: structure (§2, §4, §5) ────────────────────────────────

#[test]
fn trailing_comma_in_object_is_rejected() {
    let error = parse_err(r#"{"a":1,}"#);
    assert!(
        matches!(
            error.kind,
            JsonErrorKind::Expected {
                found: JsonToken::RightBrace,
                ..
            }
        ),
        "got {error:?}"
    );
    assert_eq!(error.position.byte(), 7);
}

#[test]
fn trailing_comma_in_array_is_rejected() {
    let error = parse_err("[1,]");
    assert!(
        matches!(
            error.kind,
            JsonErrorKind::Expected {
                found: JsonToken::RightBracket,
                ..
            }
        ),
        "got {error:?}"
    );
    assert_eq!(error.position.byte(), 3);
}

#[test]
fn leading_and_double_commas_are_rejected() {
    assert!(matches!(
        parse_err("[,1]").kind,
        JsonErrorKind::Expected { .. }
    ));
    assert!(matches!(
        parse_err("[1,,2]").kind,
        JsonErrorKind::Expected { .. }
    ));
    assert!(matches!(
        parse_err(r#"{,"a":1}"#).kind,
        JsonErrorKind::Expected { .. }
    ));
}

#[test]
fn missing_separators_are_rejected() {
    let error = parse_err(r#"{"a" 1}"#);
    assert!(matches!(
        error.kind,
        JsonErrorKind::Expected {
            expected: "':'",
            found: JsonToken::Number,
        }
    ));

    let error = parse_err("[1 2]");
    assert!(matches!(
        error.kind,
        JsonErrorKind::Expected {
            found: JsonToken::Number,
            ..
        }
    ));
}

#[test]
fn non_string_member_names_are_rejected() {
    assert!(matches!(
        parse_err("{1:2}").kind,
        JsonErrorKind::Expected {
            found: JsonToken::Number,
            ..
        }
    ));
    assert!(matches!(
        parse_err("{true:1}").kind,
        JsonErrorKind::Expected {
            found: JsonToken::True,
            ..
        }
    ));
    assert!(matches!(
        parse_err("{:1}").kind,
        JsonErrorKind::Expected {
            found: JsonToken::Colon,
            ..
        }
    ));
}

#[test]
fn missing_values_are_rejected() {
    assert!(matches!(
        parse_err(r#"{"a":}"#).kind,
        JsonErrorKind::Expected {
            found: JsonToken::RightBrace,
            ..
        }
    ));
    assert!(matches!(
        parse_err(r#"{"a":,}"#).kind,
        JsonErrorKind::Expected {
            found: JsonToken::Comma,
            ..
        }
    ));
}

#[test]
fn unclosed_containers_are_rejected_as_unexpected_eof() {
    assert!(matches!(
        parse_err("[1").kind,
        JsonErrorKind::UnexpectedEof { .. }
    ));
    assert!(matches!(
        parse_err(r#"{"a":1"#).kind,
        JsonErrorKind::UnexpectedEof { .. }
    ));
    assert!(matches!(
        parse_err("{").kind,
        JsonErrorKind::UnexpectedEof { .. }
    ));
    assert!(matches!(
        parse_err(r#"{"a""#).kind,
        JsonErrorKind::UnexpectedEof { expected: "':'" }
    ));
}

#[test]
fn stray_closers_are_rejected() {
    assert!(matches!(
        parse_err("]").kind,
        JsonErrorKind::Expected {
            found: JsonToken::RightBracket,
            ..
        }
    ));
    assert!(matches!(
        parse_err("}").kind,
        JsonErrorKind::Expected {
            found: JsonToken::RightBrace,
            ..
        }
    ));
    assert!(matches!(
        parse_err(",").kind,
        JsonErrorKind::Expected {
            found: JsonToken::Comma,
            ..
        }
    ));
}

#[test]
fn empty_and_whitespace_only_inputs_are_rejected() {
    let error = parse_err("");
    assert!(matches!(error.kind, JsonErrorKind::UnexpectedEof { .. }));
    assert_eq!(error.position.byte(), 0);

    let error = parse_err("  \r\n ");
    assert!(matches!(error.kind, JsonErrorKind::UnexpectedEof { .. }));
    assert_eq!(error.position.byte(), 5);
}

#[test]
fn trailing_content_is_rejected() {
    let error = parse_err("null null");
    assert!(matches!(
        error.kind,
        JsonErrorKind::TrailingContent {
            found: JsonToken::Null,
        }
    ));
    assert_eq!(error.position.byte(), 5);

    assert!(matches!(
        parse_err("1 2").kind,
        JsonErrorKind::TrailingContent { .. }
    ));
    assert!(matches!(
        parse_err("{} []").kind,
        JsonErrorKind::TrailingContent { .. }
    ));
    assert!(matches!(
        parse_err(r#"{"a":1}}"#).kind,
        JsonErrorKind::TrailingContent { .. }
    ));
}

#[test]
fn error_positions_report_line_and_column() {
    let error = parse_err("{\n  \"a\": 01\n}");
    let JsonErrorKind::Lex(lex) = &error.kind else {
        panic!("expected a lexical error, got {error:?}");
    };
    assert!(matches!(lex.kind, LexErrorKind::Unrecognized));
    // The second digit of `01` sits on line 2, and its precise byte is reported.
    assert_eq!(error.position.byte(), 10);
    assert_eq!(error.position.line(), 2);
    assert_eq!(error.position.column(), 9);
}
