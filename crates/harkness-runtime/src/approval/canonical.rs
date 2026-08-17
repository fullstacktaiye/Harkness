//! The frozen canonical encoding of a tool input, and the hash bound to it.
//!
//! An `ExactCall` grant is only worth anything if changing the input it
//! authorized produces a different identity. That makes this module's encoding a
//! security boundary rather than a formatting preference: two inputs must hash
//! equal exactly when they are the same value, and one recorded hash has to keep
//! meaning the same thing across releases.
//!
//! # The encoding
//!
//! [`canonical_input`] emits one spelling per JSON value:
//!
//! - `null`, `true`, and `false` as themselves.
//! - Integers in decimal, with no plus sign and no leading zeros.
//! - Other numbers as the shortest decimal that round-trips through `f64`,
//!   always carrying a `.` or an `e`. A number that is not finite is refused
//!   rather than encoded — see below.
//! - Strings quoted, escaping only what JSON requires: `"`, `\`, and the control
//!   characters below `U+0020`, using the short escapes where they exist and
//!   `\u00xx` in lowercase hex otherwise. Every other character, non-ASCII
//!   included, is emitted as its own UTF-8 bytes rather than as an escape, so
//!   one string has one spelling.
//! - Arrays in their own order, because array order is part of the value.
//! - Objects with their keys sorted by *UTF-8 byte order* and no repeated key.
//!
//! No insignificant whitespace appears anywhere, so the encoding is a function
//! of the value alone and never of how the value was parsed or printed.
//!
//! # Why a non-finite number is refused rather than encoded
//!
//! An infinity or a NaN has no JSON spelling: `serde_json` serializes both as
//! `null`, so two visibly different inputs would canonicalize identically —
//! precisely the collision this module exists to prevent. Encoding one is
//! therefore an [`ApprovalError::UncanonicalizableInput`] naming the field
//! rather than a value with a canonical form, which is why
//! [`canonical_input_hash`] is fallible.
//!
//! As this workspace is configured today no such [`Value`] can be built: the
//! parser refuses `1e999` as out of range and `serde_json::Number::from_f64`
//! refuses both infinities and NaN. The guard is not therefore decoration.
//! `arbitrary_precision` is a `serde_json` *feature*, and Cargo unifies
//! features across a workspace, so any dependency switching it on would make
//! `Number` hold raw decimal text — reopening both doors at once, in a build
//! that compiled cleanly. The same reasoning already governs `jsonschema`'s
//! retrieval features. A hash that silently started folding two inputs together
//! is not a failure anybody would notice.
//!
//! # Why the hash is domain-separated and length-framed
//!
//! [`canonical_input_hash`] absorbs [`CANONICAL_INPUT_DOMAIN`] and then the
//! canonical text, each as its length followed by its bytes. Framing makes the
//! concatenation injective, and the domain constant carries the version, so a
//! future encoding is a new constant and a new hash rather than a silent change
//! in what an old recorded hash means.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

use super::ApprovalError;

/// Domain constant naming this hash and the version of its encoding.
///
/// Changing the encoding means publishing a new constant beside this one, never
/// editing this string: every stored `input_hash` was derived under it.
pub const CANONICAL_INPUT_DOMAIN: &str = "harkness.approval.canonical-input.v1";

/// The exact-request identity an `ExactCall` grant is bound to.
///
/// A SHA-256 over the canonical encoding of the *validated* tool input, which
/// means every field the tool will actually deserialize is covered. Changing any
/// byte of any field yields a different value, so a grant obtained for one input
/// cannot transfer to another.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InputHash([u8; 32]);

impl InputHash {
    /// Length of the hexadecimal spelling.
    const HEX_LENGTH: usize = 64;

    /// The raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The lowercase hexadecimal spelling stored in `approvals.input_hash`.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut hex = String::with_capacity(Self::HEX_LENGTH);
        for byte in self.0 {
            fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"))
                .expect("writing to a String cannot fail");
        }
        hex
    }

    /// Parses the lowercase hexadecimal spelling.
    ///
    /// Uppercase is refused rather than folded: a hash is compared for equality
    /// as text in a column, and two spellings of one digest would mean an
    /// `ExactCall` grant could fail to match the request it was granted for.
    ///
    /// The value is read as bytes and the `&str` is never sliced. A length
    /// check counts bytes while `str` indexing counts characters, so a 64-*byte*
    /// value carrying a multi-byte character would put a slice boundary inside
    /// one and panic. Every caller here is handling something from outside the
    /// process — a hand-edited column, a CLI argument, a value off the GUI
    /// bridge — and a panic is not a refusal.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::MalformedInputHash`] when the value is not
    /// exactly 64 lowercase hexadecimal characters.
    pub fn parse(value: &str) -> Result<Self, ApprovalError> {
        let malformed = |reason| ApprovalError::MalformedInputHash {
            value: value.to_owned(),
            reason,
        };
        let spelling = value.as_bytes();
        if spelling.len() != Self::HEX_LENGTH {
            return Err(malformed("it is not 64 hexadecimal characters long"));
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(spelling.chunks_exact(2)) {
            for digit in pair {
                let nibble = match digit {
                    b'0'..=b'9' => digit - b'0',
                    b'a'..=b'f' => digit - b'a' + 10,
                    _ => return Err(malformed("it is not spelled in lowercase hexadecimal")),
                };
                *byte = (*byte << 4) | nibble;
            }
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for InputHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for InputHash {
    type Err = ApprovalError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for InputHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for InputHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelling = String::deserialize(deserializer)?;
        Self::parse(&spelling).map_err(serde::de::Error::custom)
    }
}

/// Hashes a validated tool input under [`CANONICAL_INPUT_DOMAIN`].
///
/// **This algorithm is frozen.** Its output is persisted in
/// `approvals.input_hash` and compared against a fresh derivation on every
/// candidate call, so a change to the encoding or the framing invalidates every
/// stored grant. Publish a new domain constant instead; the committed
/// canonicalization fixture is what fails when this drifts.
///
/// # Errors
///
/// Returns [`ApprovalError::UncanonicalizableInput`] when the value holds a
/// number with no finite canonical spelling.
pub fn canonical_input_hash(input: &Value) -> Result<InputHash, ApprovalError> {
    let canonical = canonical_input(input)?;
    let mut hasher = Sha256::new();
    absorb(&mut hasher, CANONICAL_INPUT_DOMAIN.as_bytes());
    absorb(&mut hasher, canonical.as_bytes());
    Ok(InputHash(hasher.finalize().into()))
}

/// Renders a value in the frozen canonical form described by this module.
///
/// Exposed beside the hash so a test — and a human debugging why two inputs
/// disagree — can see the exact bytes that were hashed.
///
/// # Errors
///
/// As [`canonical_input_hash`].
pub fn canonical_input(input: &Value) -> Result<String, ApprovalError> {
    let mut canonical = String::new();
    let mut pointer = String::new();
    write_value(&mut canonical, input, &mut pointer)?;
    Ok(canonical)
}

/// Absorbs one length-framed component, so concatenation stays injective.
fn absorb(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn write_value(
    canonical: &mut String,
    value: &Value,
    pointer: &mut String,
) -> Result<(), ApprovalError> {
    match value {
        Value::Null => canonical.push_str("null"),
        Value::Bool(true) => canonical.push_str("true"),
        Value::Bool(false) => canonical.push_str("false"),
        Value::Number(number) => write_number(canonical, number, pointer)?,
        Value::String(text) => write_string(canonical, text),
        Value::Array(items) => {
            canonical.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    canonical.push(',');
                }
                let restore = pointer.len();
                pointer.push('/');
                fmt::Write::write_fmt(pointer, format_args!("{index}"))
                    .expect("writing to a String cannot fail");
                write_value(canonical, item, pointer)?;
                pointer.truncate(restore);
            }
            canonical.push(']');
        }
        Value::Object(fields) => {
            // Sorted by the exact key bytes rather than by any locale or
            // character-wise ordering, so the order is a property of the value
            // and not of the platform that encoded it. Never inherited from the
            // map type: `serde_json::Map` is a `BTreeMap` only until some crate
            // in the workspace enables `preserve_order`, which Cargo then
            // unifies onto every member — `agent-client-protocol-schema`
            // requires it (ADR-0010), so the map here is an `IndexMap` and this
            // sort is what every stored `input_hash` has always depended on.
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            canonical.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    canonical.push(',');
                }
                write_string(canonical, key);
                canonical.push(':');
                let restore = pointer.len();
                pointer.push('/');
                pointer.push_str(&escape_pointer_token(key));
                write_value(canonical, &fields[key], pointer)?;
                pointer.truncate(restore);
            }
            canonical.push('}');
        }
    }
    Ok(())
}

fn write_number(
    canonical: &mut String,
    number: &Number,
    pointer: &str,
) -> Result<(), ApprovalError> {
    if let Some(value) = number.as_u64() {
        fmt::Write::write_fmt(canonical, format_args!("{value}"))
            .expect("writing to a String cannot fail");
        return Ok(());
    }
    if let Some(value) = number.as_i64() {
        fmt::Write::write_fmt(canonical, format_args!("{value}"))
            .expect("writing to a String cannot fail");
        return Ok(());
    }
    let uncanonicalizable = |reason| ApprovalError::UncanonicalizableInput {
        pointer: if pointer.is_empty() {
            "".to_owned()
        } else {
            pointer.to_owned()
        },
        reason,
    };
    let value = number
        .as_f64()
        .ok_or_else(|| uncanonicalizable(NO_REPRESENTABLE_VALUE))?;
    canonical.push_str(&encode_double(value).ok_or_else(|| uncanonicalizable(NOT_FINITE))?);
    Ok(())
}

/// Reason reported for a number this build cannot read as a value at all.
const NO_REPRESENTABLE_VALUE: &str = "it is a number with no representable value";

/// Reason reported for an infinity or a NaN.
const NOT_FINITE: &str = "it is not finite, and a non-finite number has no canonical JSON spelling";

/// Encodes a double, or refuses one with no JSON spelling.
///
/// `{:?}` is the shortest decimal that round-trips through `f64` and always
/// carries a `.` or an `e`, so an integral double never collides with the
/// integer of the same value.
fn encode_double(value: f64) -> Option<String> {
    value.is_finite().then(|| format!("{value:?}"))
}

fn write_string(canonical: &mut String, value: &str) {
    canonical.push('"');
    for character in value.chars() {
        match character {
            '"' => canonical.push_str("\\\""),
            '\\' => canonical.push_str("\\\\"),
            '\u{08}' => canonical.push_str("\\b"),
            '\u{0c}' => canonical.push_str("\\f"),
            '\n' => canonical.push_str("\\n"),
            '\r' => canonical.push_str("\\r"),
            '\t' => canonical.push_str("\\t"),
            control if control < '\u{20}' => {
                fmt::Write::write_fmt(canonical, format_args!("\\u{:04x}", control as u32))
                    .expect("writing to a String cannot fail");
            }
            other => canonical.push(other),
        }
    }
    canonical.push('"');
}

/// Escapes an object key into one RFC 6901 pointer token.
fn escape_pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::ApprovalError;
    use super::{
        CANONICAL_INPUT_DOMAIN, InputHash, canonical_input, canonical_input_hash, encode_double,
        escape_pointer_token,
    };

    /// The committed fixture that pins this encoding across releases.
    const FROZEN: &str = include_str!("fixtures/canonical-input-v1.json");

    /// The reference input the fixture is derived from.
    ///
    /// Deliberately awkward: keys out of order, nesting, an empty object and an
    /// empty array, a negative integer, a fractional number, a `null`, escapes,
    /// and a non-ASCII string. Every rule this module states is exercised by it,
    /// so a change to any of them moves the frozen hash.
    fn reference_input() -> Value {
        json!({
            "path": "crates/harkness-runtime/src/store/mod.rs",
            "contents": "fn main() {\n\t\"quoted\\\\\"\n}",
            "author": "Ada Lovelace — 愛",
            "range": {"start": 1, "end": -40},
            "ratio": 0.5,
            "flags": [true, false, null],
            "empty_object": {},
            "empty_array": [],
            "Capitalized": "byte order puts this before every lowercase key"
        })
    }

    fn hash(value: &Value) -> InputHash {
        canonical_input_hash(value).unwrap()
    }

    #[test]
    fn the_frozen_fixture_pins_the_encoding_and_the_hash() {
        let fixture: Value = serde_json::from_str(FROZEN).unwrap();
        assert_eq!(
            fixture["domain"].as_str().unwrap(),
            CANONICAL_INPUT_DOMAIN,
            "the fixture must be derived under the current domain constant"
        );

        let input = &fixture["input"];
        assert_eq!(
            input,
            &reference_input(),
            "the committed input is the pin; changing the reference input here \
             without publishing a new domain constant moves the hash silently"
        );
        assert_eq!(
            canonical_input(input).unwrap(),
            fixture["canonical"].as_str().unwrap(),
            "the canonical encoding changed; publish a new domain constant and \
             a new fixture rather than editing this one"
        );
        assert_eq!(
            hash(input).to_hex(),
            fixture["hash"].as_str().unwrap(),
            "the frozen hash changed; every stored grant is bound to it"
        );
    }

    #[test]
    fn key_order_and_whitespace_do_not_change_the_hash() {
        let first: Value =
            serde_json::from_str(r#"{"alpha": 1, "beta": {"gamma": [1, 2], "delta": null}}"#)
                .unwrap();
        let second: Value = serde_json::from_str(
            "{\n  \"beta\" : { \"delta\" : null , \"gamma\" : [ 1 , 2 ] } ,\n  \"alpha\" : 1\n}",
        )
        .unwrap();

        assert_eq!(
            canonical_input(&first).unwrap(),
            canonical_input(&second).unwrap()
        );
        assert_eq!(hash(&first), hash(&second));
    }

    #[test]
    fn changing_one_byte_of_one_field_changes_the_hash() {
        let original = json!({"path": "src/lib.rs", "start": 1, "end": 40});
        let baseline = hash(&original);

        for changed in [
            json!({"path": "src/lib.rt", "start": 1, "end": 40}),
            json!({"path": "src/lib.rs", "start": 2, "end": 40}),
            json!({"path": "src/lib.rs", "start": 1, "end": 41}),
            json!({"path": "src/lib.rs", "start": 1, "end": 40, "extra": null}),
            json!({"path": "src/lib.rs", "start": 1}),
            json!({"path": "src/lib.rs ", "start": 1, "end": 40}),
        ] {
            assert_ne!(hash(&changed), baseline, "{changed} should not match");
        }
    }

    #[test]
    fn array_order_is_part_of_the_value_and_object_order_is_not() {
        assert_ne!(hash(&json!([1, 2])), hash(&json!([2, 1])));
        assert_eq!(
            hash(&json!({"a": 1, "b": 2})),
            hash(&json!({"b": 2, "a": 1}))
        );
    }

    #[test]
    fn types_that_render_alike_still_hash_differently() {
        // Each pair would collide under an encoding that leaned on `to_string`
        // or on a numeric coercion.
        assert_ne!(hash(&json!("1")), hash(&json!(1)));
        assert_ne!(hash(&json!(1)), hash(&json!(1.0)));
        assert_ne!(hash(&json!(null)), hash(&json!("null")));
        assert_ne!(hash(&json!([])), hash(&json!({})));
        assert_ne!(hash(&json!(0.0)), hash(&json!(-0.0)));
    }

    #[test]
    fn unicode_and_control_characters_have_one_spelling_each() {
        assert_eq!(
            canonical_input(&json!("héllo → ✅")).unwrap(),
            "\"héllo → ✅\""
        );
        assert_eq!(
            canonical_input(&json!("a\tb\nc\\d\"e\u{1}f")).unwrap(),
            r#""a\tb\nc\\d\"e\u0001f""#
        );
        // Two spellings of one string in the source JSON reach one canonical
        // form, so an agent cannot change an input's hash by escaping it
        // differently.
        let escaped: Value = serde_json::from_str(r#"{"note":"é"}"#).unwrap();
        assert_eq!(hash(&escaped), hash(&json!({"note": "é"})));
    }

    #[test]
    fn a_non_finite_double_has_no_canonical_spelling() {
        // Both would serialize as `null`, folding two different inputs onto one
        // hash, so neither may be encoded.
        assert_eq!(encode_double(f64::INFINITY), None);
        assert_eq!(encode_double(f64::NEG_INFINITY), None);
        assert_eq!(encode_double(f64::NAN), None);

        // Every finite double keeps a spelling that cannot be read as an
        // integer, so `1` and `1.0` stay distinct values.
        assert_eq!(encode_double(1.0).unwrap(), "1.0");
        assert_eq!(encode_double(-0.5).unwrap(), "-0.5");
        assert_eq!(encode_double(1e300).unwrap(), "1e300");
    }

    #[test]
    fn neither_door_to_a_non_finite_number_is_currently_open() {
        // This is why the refusal above is unreachable through `Value` today,
        // and the assertion that fails first if `arbitrary_precision` is ever
        // unified into this workspace and reopens both.
        assert!(serde_json::from_str::<Value>(r#"{"upper": 1e999}"#).is_err());
        assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
        assert!(serde_json::Number::from_f64(f64::NAN).is_none());
        // `to_value` is the third door and it does not fail: it degrades a
        // non-finite float to `null`. That still yields no non-finite `Number`,
        // and it is a fold this module cannot undo — but it is `serde_json`'s,
        // not the canonical encoding's, and it cannot happen to a tool input,
        // which reaches the pipeline as JSON text the parser already refused.
        assert_eq!(serde_json::to_value(f64::INFINITY).unwrap(), Value::Null);
    }

    #[test]
    fn a_pointer_names_an_array_index_and_escapes_an_awkward_key() {
        // The tokens a refusal is located by. `~` and `/` are the two
        // characters RFC 6901 gives its own meaning, so a key containing either
        // has to be escaped or the pointer names a different place entirely.
        assert_eq!(escape_pointer_token("path"), "path");
        assert_eq!(escape_pointer_token("a/b"), "a~1b");
        assert_eq!(escape_pointer_token("a~b"), "a~0b");
        assert_eq!(escape_pointer_token("~/"), "~0~1");
    }

    #[test]
    fn a_refusal_carries_the_pointer_and_the_reason_it_was_given() {
        let error = ApprovalError::UncanonicalizableInput {
            pointer: "/limit/upper".to_owned(),
            reason: super::NOT_FINITE,
        };
        assert_eq!(error.kind(), "approval_uncanonicalizable_input");
        assert!(error.to_string().contains("/limit/upper"));
        assert!(error.to_string().contains("non-finite"));
    }

    #[test]
    fn hashes_round_trip_through_their_hexadecimal_spelling() {
        let digest = hash(&json!({"path": "src/lib.rs"}));
        let hex = digest.to_hex();

        assert_eq!(hex.len(), 64);
        assert_eq!(InputHash::parse(&hex).unwrap(), digest);
        assert_eq!(hex.parse::<InputHash>().unwrap(), digest);
        assert_eq!(
            serde_json::from_str::<InputHash>(&format!("\"{hex}\"")).unwrap(),
            digest
        );
        assert_eq!(
            serde_json::to_string(&digest).unwrap(),
            format!("\"{hex}\"")
        );
    }

    #[test]
    fn an_uppercase_or_short_hash_spelling_is_refused() {
        let hex = hash(&json!({})).to_hex();
        for spelling in [hex.to_uppercase(), hex[..63].to_owned(), format!("{hex}0")] {
            let error = InputHash::parse(&spelling).unwrap_err();
            assert_eq!(error.kind(), "approval_malformed_input_hash");
        }
    }

    #[test]
    fn a_non_ascii_hash_spelling_is_refused_rather_than_panicking() {
        // Sixty-four *bytes* carrying a two-byte character, so the pair
        // boundaries land inside it. A length check counts bytes and `str`
        // indexing counts characters, so slicing here would panic — and every
        // caller of `parse` is handling a value from outside the process.
        let straddling = format!("a\u{e9}{}", "0".repeat(61));
        assert_eq!(straddling.len(), 64);
        assert_ne!(straddling.chars().count(), 64);

        for spelling in [
            straddling,
            "\u{e9}".repeat(32),
            format!("{}\u{20ac}", "0".repeat(61)),
        ] {
            let error = InputHash::parse(&spelling).unwrap_err();
            assert_eq!(error.kind(), "approval_malformed_input_hash");
            assert!(
                error.to_string().contains("lowercase hexadecimal"),
                "{error}"
            );
        }
    }

    #[test]
    fn every_hexadecimal_digit_decodes_to_the_nibble_it_names() {
        let spelling = "0123456789abcdef".repeat(4);
        let parsed = InputHash::parse(&spelling).unwrap();
        assert_eq!(parsed.to_hex(), spelling);
        assert_eq!(
            parsed.as_bytes()[..8],
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
        );
    }

    /// Rewrites the frozen canonicalization fixture.
    ///
    /// Run deliberately, and only when a *new* domain constant is published:
    /// `cargo test -p harkness-runtime regenerate_the_frozen_canonicalization_fixture -- --ignored`.
    /// Rewriting it to make a changed encoding pass is the one thing this
    /// fixture exists to prevent — every stored `input_hash` was derived under
    /// the encoding it pins.
    #[test]
    #[ignore = "rewrites a committed fixture; run only when a new hash domain is published"]
    fn regenerate_the_frozen_canonicalization_fixture() {
        let input = reference_input();
        let fixture = serde_json::json!({
            "domain": CANONICAL_INPUT_DOMAIN,
            "input": input,
            "canonical": canonical_input(&input).unwrap(),
            "hash": hash(&input).to_hex(),
        });
        let destination = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/approval/fixtures/canonical-input-v1.json");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(
            destination,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn the_domain_constant_takes_part_in_the_digest() {
        // A digest over the canonical text alone would let a hash derived for
        // another purpose be replayed as an approval binding.
        use sha2::{Digest, Sha256};
        let canonical = canonical_input(&json!({"path": "src/lib.rs"})).unwrap();
        let undomained = Sha256::digest(canonical.as_bytes());
        assert_ne!(
            hash(&json!({"path": "src/lib.rs"})).as_bytes().as_slice(),
            undomained.as_slice()
        );
    }
}
