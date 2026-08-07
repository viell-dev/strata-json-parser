//! Source fidelity: trivia retention, span correctness, and zero-copy access.

use strata_json_parser::{Document, JsonError, JsonParser, JsonToken, TriviaKind, Value};
use strata_lexer::Span;

fn parse(input: &str) -> Result<Document, JsonError> {
    JsonParser::new().parse_str(input)
}

/// Collects every retained span — leading trivia and lexemes, in source order — for the whole
/// document.
fn all_spans(document: &Document) -> Vec<Span> {
    let mut spans = Vec::new();

    document.root().for_each_token(&mut |token| {
        for trivia in token.leading() {
            spans.push(trivia.span());
        }

        spans.push(token.span());
    });

    for trivia in document.trailing_trivia() {
        spans.push(trivia.span());
    }

    spans
}

/// Rebuilds the source text from the retained spans.
fn reconstruct(document: &Document) -> Option<Vec<u8>> {
    let mut out = Vec::new();

    for span in all_spans(document) {
        out.extend_from_slice(document.bytes(span)?);
    }

    Some(out)
}

#[test]
fn leading_and_trailing_whitespace_is_retained() -> Result<(), JsonError> {
    let document = parse("  null  ")?;

    let leading = document.root().first_token().leading();
    let [trivia] = leading else {
        panic!("expected one leading trivia piece, got {leading:?}");
    };
    assert_eq!(trivia.kind(), TriviaKind::Whitespace);
    assert_eq!(trivia.span(), Span::from_start_len(0, 2));

    let [trailing] = document.trailing_trivia() else {
        panic!("expected one trailing trivia piece");
    };
    assert_eq!(trailing.kind(), TriviaKind::Whitespace);
    assert_eq!(trailing.span(), Span::from_start_len(6, 2));

    Ok(())
}

#[test]
fn whitespace_between_tokens_attaches_to_the_following_token() -> Result<(), JsonError> {
    let document = parse("{ \"a\" : 1 }")?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let [member] = object.members() else {
        panic!("expected one member");
    };

    // Each significant token carries the whitespace run that preceded it.
    assert_eq!(member.name().leading().len(), 1);
    assert_eq!(member.colon().leading().len(), 1);
    assert_eq!(member.value().first_token().leading().len(), 1);
    assert_eq!(object.close().leading().len(), 1);
    // The open brace is the first byte: nothing precedes it.
    assert!(object.open().leading().is_empty());

    Ok(())
}

#[test]
fn blank_lines_are_observable_in_trivia() -> Result<(), JsonError> {
    // A blank line between the "a" and "b" members: the human grouping signal the ecosystem
    // preserves.
    let document = parse("{\n  \"a\": 1,\n\n  \"b\": 2\n}")?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let [member_a, member_b] = object.members() else {
        panic!("expected two members");
    };

    let [gap_a] = member_a.name().leading() else {
        panic!("expected leading trivia on \"a\"");
    };
    assert_eq!(gap_a.line_breaks(), 1);
    assert_eq!(gap_a.blank_lines(), 0);

    let [gap_b] = member_b.name().leading() else {
        panic!("expected leading trivia on \"b\"");
    };
    assert_eq!(gap_b.line_breaks(), 2);
    assert_eq!(gap_b.blank_lines(), 1);

    Ok(())
}

#[test]
fn spans_reconstruct_the_source_byte_for_byte() -> Result<(), JsonError> {
    let input = " {\n  \"key\" : [ 1 ,\ttrue, \"txt\" ] ,\r\n  \"n\" : { } \n}  ";
    let document = parse(input)?;

    let Some(rebuilt) = reconstruct(&document) else {
        panic!("every span must resolve against the retained source");
    };
    assert_eq!(rebuilt, input.as_bytes());

    Ok(())
}

#[test]
fn spans_are_contiguous_and_cover_the_input() -> Result<(), JsonError> {
    let input = "[ 0, { \"a\" : null } ]\n";
    let document = parse(input)?;

    let mut cursor: u64 = 0;

    for span in all_spans(&document) {
        assert_eq!(span.start(), cursor, "gap or overlap before {span:?}");
        assert!(span.end() >= span.start());
        cursor = span.end();
    }

    assert_eq!(cursor, u64::try_from(input.len()).unwrap_or(u64::MAX));

    Ok(())
}

#[test]
fn token_spans_address_their_exact_lexemes() -> Result<(), JsonError> {
    let input = r#"{"key": [12.5e-1, "value"]}"#;
    let document = parse(input)?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let [member] = object.members() else {
        panic!("expected one member");
    };

    assert_eq!(document.token_bytes(member.name()), Some(&b"\"key\""[..]));
    assert_eq!(document.token_bytes(member.colon()), Some(&b":"[..]));

    let Value::Array(array) = member.value() else {
        panic!("expected an array value");
    };
    let [number, string] = array.elements() else {
        panic!("expected two elements");
    };
    assert_eq!(document.bytes(number.value().span()), Some(&b"12.5e-1"[..]));
    assert_eq!(
        document.bytes(string.value().span()),
        Some(&b"\"value\""[..])
    );

    // Value::span covers first to last lexeme byte.
    assert_eq!(
        document.bytes(member.value().span()),
        Some(&b"[12.5e-1, \"value\"]"[..])
    );

    Ok(())
}

#[test]
fn positions_track_lines_and_columns() -> Result<(), JsonError> {
    let document = parse("[\n  1,\n  22\n]")?;
    let Value::Array(array) = document.root() else {
        panic!("expected an array");
    };

    assert_eq!(array.open().start().line(), 1);
    assert_eq!(array.open().start().column(), 1);

    let [one, twentytwo] = array.elements() else {
        panic!("expected two elements");
    };
    let one = one.value().first_token();
    assert_eq!((one.start().line(), one.start().column()), (2, 3));

    let twentytwo = twentytwo.value().first_token();
    assert_eq!(
        (twentytwo.start().line(), twentytwo.start().column()),
        (3, 3)
    );
    assert_eq!((twentytwo.end().line(), twentytwo.end().column()), (3, 5));

    assert_eq!(
        (array.close().start().line(), array.close().start().column()),
        (4, 1)
    );

    Ok(())
}

#[test]
fn eof_token_kind_is_not_part_of_the_tree() -> Result<(), JsonError> {
    // The CST contains only significant tokens; EOF exists solely as the parse terminator.
    let document = parse("1")?;
    let mut kinds = Vec::new();
    document
        .root()
        .for_each_token(&mut |token| kinds.push(token.kind));
    assert_eq!(kinds, [JsonToken::Number]);

    Ok(())
}

#[test]
fn source_refs_survive_store_reclamation() -> Result<(), JsonError> {
    let mut document = parse(r#"{"keep": 12345}"#)?;
    let Value::Object(object) = document.root() else {
        panic!("expected an object");
    };
    let [member] = object.members() else {
        panic!("expected one member");
    };
    let number_span = member.value().span();

    let Some(source_ref) = document.source_ref(number_span) else {
        panic!("span must resolve to a source ref");
    };
    assert!(source_ref.is_loaned());
    assert_eq!(source_ref.to_vec(), b"12345");

    // Owner-side reclamation promotes surviving refs; plain span access is gone afterwards,
    // the promoted handle still resolves.
    let retired = document.source_mut().retire_all();
    assert_eq!(retired, 1);
    assert!(document.bytes(number_span).is_none());
    assert_eq!(source_ref.resolve(), b"12345");
    assert!(source_ref.is_owned());

    Ok(())
}
