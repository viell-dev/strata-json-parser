# strata-json-parser

Strict RFC 8259 JSON parser for the Strata parser ecosystem: `strata-json-rules` tokens parsed
into a trivia-retaining, zero-copy CST. The CST and grammar parts are reusable by
superset-language parsers.

## Status

This crate is in initial development. The public API is not settled yet.

## Usage

```rust
use strata_json_parser::{JsonError, JsonParser, Value};

fn main() -> Result<(), JsonError> {
    let document = JsonParser::new().parse_str(r#"{"greeting": "hello",  "count": 3}"#)?;

    let Value::Object(object) = document.root() else {
        panic!("expected an object root");
    };
    let [greeting, count] = object.members() else {
        panic!("expected two members");
    };

    // Nothing is decoded: the CST keeps raw spans into the retained source.
    assert_eq!(
        document.token_bytes(greeting.name()),
        Some(&b"\"greeting\""[..])
    );

    // Trivia is retained, attached to the token it precedes.
    let [gap] = count.name().leading() else {
        panic!("expected the double space before \"count\"");
    };
    assert_eq!(document.trivia_bytes(gap), Some(&b"  "[..]));

    Ok(())
}
```

Superset parsers do not wrap `JsonParser`; they implement the `grammar::Syntax` trait and
instantiate the generic CST with their own token and trivia kinds. Resolver layers decode
string and number lexemes through the `decode` module. See
[docs/strata-json-parser.md](docs/strata-json-parser.md) for the design reference.

## Minimum Supported Rust Version

The MSRV is **1.87.0**.

## License

Licensed under the Mozilla Public License, Version 2.0 (see [LICENSE](LICENSE)
or <https://www.mozilla.org/en-US/MPL/2.0/>).

MPL-2.0 is a file-level weak copyleft license: its obligations attach to this crate's files (in
source and compiled form) and to modifications of them, not to your own source files that merely
use this crate. Distributing a binary that includes this crate obliges you to tell recipients
that the crate is included and how to obtain its source; for an unmodified crate, pointing them
at the exact crates.io release is a reasonable way to do that. Example material (documentation
code examples, `examples/`, integration tests, and benchmarks) is additionally licensed under
[MIT-0](LICENSE-MIT-0), so it can be copied freely. See
[LICENSE-CLARIFICATION.md](LICENSE-CLARIFICATION.md) for the maintainers' full statement of
intent regarding normal Rust usage (linking, monomorphization, macro expansion), binary
distribution, and generated code, and [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution
policy.
