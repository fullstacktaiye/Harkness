//! The pattern rules that recognize a credential by its shape.
//!
//! Each rule is a named, individually testable pattern with one replacement
//! marker. The marker names the rule and never echoes any part of what it
//! replaced, so a record says *that* something was scrubbed and by which rule,
//! and an operator reading it learns nothing they did not already have.
//!
//! # Why fixed shapes and not entropy
//!
//! Entropy scoring finds secrets nobody described, and it also finds commit
//! hashes, base64-encoded diffs, UUIDs, minified JavaScript and compiler output.
//! A false positive here is not a nuisance: it silently rewrites the audit trail
//! a user is relying on to understand what happened, in a way nothing downstream
//! can detect or undo. Every rule below therefore keys on a shape whose issuer
//! publishes it — a URL's userinfo, an `Authorization` header, a token prefix —
//! and the one rule that matches an arbitrary string,
//! [`RedactionRule::DeclaredSecret`], matches only values this process was
//! explicitly told about (see [`secret`](super::secret)).
//!
//! # One pattern set, two engines
//!
//! [`redact_text`](super::StandardRedactor) works on `&str`, because an event
//! payload is a JSON document and its strings are UTF-8 by construction. An
//! artifact stream is arbitrary bytes — a core file, a compressed blob, a
//! process's stdout in an unknown encoding — and decoding it to run string rules
//! would corrupt everything that is not UTF-8. Both engines are therefore
//! compiled from the *same* pattern constants, one through [`regex::Regex`] and
//! one through [`regex::bytes::Regex`], so a rule cannot mean two things
//! depending on which side of the store a value arrived on.
//!
//! A byte regex in Unicode mode simply does not match inside invalid UTF-8,
//! which is the behaviour worth having: a binary artifact is left byte-identical
//! rather than mangled, and a credential written as text is still found.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::bytes::Regex as ByteRegex;
use regex::{Captures, Regex};

/// Which rule rewrote a stretch of text.
///
/// Public because it is what a marker names and what the coverage table in
/// `docs/observability.md` is written against; a contributor adding a persisted
/// channel needs to be able to say which of these reach it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum RedactionRule {
    /// A value the execution context declared secret (see [`secret`](super::secret)).
    DeclaredSecret,
    /// The userinfo component of a URL: `scheme://user:secret@host`.
    UrlUserinfo,
    /// An `Authorization` header, or a bare `Bearer`/`Basic`/`token` credential.
    Authorization,
    /// A `token=`/`password=`/`api_key=`-shaped key and value.
    CredentialParameter,
    /// A credential whose issuer publishes its prefix (`ghp_`, `AKIA`, a JWT).
    CredentialToken,
    /// A PEM private key, from its `BEGIN` line to its `END` line.
    PrivateKeyBlock,
}

impl RedactionRule {
    /// Stable snake_case name, as it appears inside a marker.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeclaredSecret => "declared_secret",
            Self::UrlUserinfo => "url_userinfo",
            Self::Authorization => "authorization",
            Self::CredentialParameter => "credential_parameter",
            Self::CredentialToken => "credential_token",
            Self::PrivateKeyBlock => "private_key_block",
        }
    }

    /// Exactly what this rule leaves behind.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::DeclaredSecret => DECLARED_SECRET_MARKER,
            Self::UrlUserinfo => URL_USERINFO_MARKER,
            Self::Authorization => AUTHORIZATION_MARKER,
            Self::CredentialParameter => CREDENTIAL_PARAMETER_MARKER,
            Self::CredentialToken => CREDENTIAL_TOKEN_MARKER,
            Self::PrivateKeyBlock => PRIVATE_KEY_BLOCK_MARKER,
        }
    }

    /// Whether an artifact's byte stream is scanned for this rule.
    ///
    /// Everything except [`PrivateKeyBlock`](Self::PrivateKeyBlock), which is
    /// the one rule that needs to see across newlines: a stream is filtered a
    /// line at a time so its memory stays bounded, and a rule spanning lines
    /// would need the whole artifact in memory to be applied honestly. The gap
    /// is named here and in `docs/observability.md` rather than papered over.
    #[must_use]
    pub const fn covers_streams(self) -> bool {
        !matches!(self, Self::PrivateKeyBlock)
    }

    /// Every rule, in the order they are applied.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::DeclaredSecret,
            Self::PrivateKeyBlock,
            Self::UrlUserinfo,
            Self::Authorization,
            Self::CredentialParameter,
            Self::CredentialToken,
        ]
    }
}

const DECLARED_SECRET_MARKER: &str = "«redacted:declared_secret»";
const URL_USERINFO_MARKER: &str = "«redacted:url_userinfo»";
const AUTHORIZATION_MARKER: &str = "«redacted:authorization»";
const CREDENTIAL_PARAMETER_MARKER: &str = "«redacted:credential_parameter»";
const CREDENTIAL_TOKEN_MARKER: &str = "«redacted:credential_token»";
const PRIVATE_KEY_BLOCK_MARKER: &str = "«redacted:private_key_block»";

/// The whole userinfo component goes, not only the password half.
///
/// `https://<token>@github.com` is the commonest credential URL there is, and in
/// `https://x-access-token:ghp_…@host` the *username* names the scheme while the
/// password is the secret — so keeping either half is a rule that leaks on the
/// shape it was most likely to meet. The host and the path survive, which is
/// what a run record actually needs in order to say where a fetch went.
///
/// The userinfo class is RFC 3986's — `unreserved`, `pct-encoded`, `sub-delims`
/// and `:` — written positively rather than as "anything but a delimiter", and
/// that is load-bearing rather than pedantic. A negated class admits `"`, so on
/// an already-encoded JSON line
/// `{"remote":"https://example.com","author":"a@b.com"}` it would match from the
/// scheme all the way across two field boundaries into the second value, and the
/// replacement would leave a line that no longer parses. Excluding the
/// characters a URL cannot contain is what keeps redaction from corrupting the
/// record it is protecting — and it makes the rule idempotent by construction,
/// because `«` is not in the class either.
const URL_USERINFO: &str = r"(?i)\b([a-z][a-z0-9+.\-]*)://[A-Za-z0-9._~%!$&'()*+,;=:-]+@";
const URL_USERINFO_REPLACEMENT: &str = "${1}://«redacted:url_userinfo»@";

/// A header's whole value, because the scheme is not the interesting part.
///
/// Written to accept both `Authorization: Bearer …` and the JSON spelling
/// `"authorization": "Bearer …"`, since process output and structured payloads
/// both carry these and a rule that knew only one shape would look like it
/// worked.
///
/// Two details of the separator and the value class are load-bearing, and both
/// exist because the diagnostic log redacts an *already-encoded* JSON line.
///
/// The optional quote either side may be escaped, because a tool that quoted a
/// command line reaches the log as `Authorization: \"Bearer …\"`. And the value
/// class refuses the backslash along with the quote, the newline and every
/// bracket, so a match cannot walk out of the field it started in. The two go
/// together: without the escaped quote in the separator, the value would match
/// the lone `\`, stop at the quote, and leave the credential in the clear beside
/// a line that no longer parses — the worst of both halves of this module's job.
const AUTHORIZATION_HEADER: &str = concat!(
    r#"(?i)\b((?:proxy-)?authorization)("#,
    r#"(?:\\?")?\s*[:=]\s*(?:\\?")?"#,
    r#")([^"\\\r\n(){}\[\]]*[^"\\\s\r\n(){}\[\]])"#,
);

/// Schemes an `Authorization` value may open with, from the IANA registry.
///
/// Present so the header rule can tell a credential from an explanation. A
/// denial recorded as `authorization: denied because the grant no longer covers
/// remote_write` is the reason a call was refused, and a rule that replaced it
/// would destroy the one field an operator needs — so a value is only scrubbed
/// when it opens with a scheme, or is a single opaque token.
const AUTHORIZATION_SCHEMES: &[&str] = &[
    "aws4-hmac-sha256",
    "basic",
    "bearer",
    "digest",
    "hoba",
    "mutual",
    "negotiate",
    "ntlm",
    "oauth",
    "scram-sha-1",
    "scram-sha-256",
    "token",
    "vapid",
];

/// A bare credential introduced by its scheme, with no header name in sight.
///
/// Guarded by [`is_credential_shaped`]: `bearer of bad news` and
/// `basic authentication` are English, and a rule that ate them would corrupt
/// far more records than it protected. Requiring one non-letter in the value is
/// what separates a token from a word.
const AUTHORIZATION_SCHEME: &str = r"(?i)\b(bearer|basic|token)\s+([A-Za-z0-9._~+/=\-]{8,})";

/// Key-and-value shapes, in URLs, environment-style lines and JSON alike.
///
/// The key list is deliberately narrow. `key=`, `id=` and `pwd=` are not on it:
/// they appear all over ordinary configuration and output — `PWD=` and `OLDPWD=`
/// are in every environment dump, and the working directory is one of the first
/// things anyone needs to reconstruct a failed run — and a false positive here
/// corrupts an audit record rather than merely annoying somebody.
///
/// There is no word boundary before the key, on purpose: the shape that matters
/// most in process output is `PGPASSWORD=…`, and a boundary would require the
/// key to stand alone. What keeps that from over-matching is the *separator* —
/// a key has to be followed by `:` or `=` and a value, so `secretariat=racehorse`
/// and `mypasswordfield` are both left alone.
const CREDENTIAL_PARAMETER: &str = concat!(
    r#"(?i)(access[_-]?token|api[_-]?key|apikey|auth[_-]?token|client[_-]?secret"#,
    r#"|id[_-]?token|passphrase|password|passwd|private[_-]?key|refresh[_-]?token"#,
    r#"|secret[_-]?access[_-]?key|secret[_-]?key|session[_-]?token|secret|token)"#,
    r#"((?:\\?")?\s*[:=]\s*(?:\\?")?)([^"'\\\s&;,(){}\[\]]+)"#,
);
const CREDENTIAL_PARAMETER_REPLACEMENT: &str = "${1}${2}«redacted:credential_parameter»";

/// Credentials whose issuers publish a fixed prefix.
///
/// A prefix list, never an entropy test: ordinary base64 does not begin `ghp_`,
/// and a commit hash does not begin `AKIA`. The JWT arm is the one shape that is
/// not a vendor prefix, and it is here because `eyJ` — three characters that
/// decode to the start of a JSON object — plus two dot-separated segments is a
/// bearer token by construction.
const CREDENTIAL_TOKEN: &str = r"(?x)
      \b gh[pousr]_ [A-Za-z0-9]{36,251}
    | \b github_pat_ [A-Za-z0-9]{22} _ [A-Za-z0-9]{59}
    | \b glpat- [A-Za-z0-9_\-]{20,}
    | \b xox[abposr]- [A-Za-z0-9\-]{10,}
    | \b (?:AKIA|ASIA|ABIA|ACCA) [A-Z0-9]{16} \b
    | \b A3T [A-Z0-9]{17} \b
    | \b AIza [A-Za-z0-9_\-]{35}
    | \b sk-(?:ant|proj|live|test)- [A-Za-z0-9_\-]{16,}
    | \b sk_(?:live|test)_ [A-Za-z0-9]{16,}
    | \b npm_ [A-Za-z0-9]{36}
    | \b eyJ[A-Za-z0-9_\-]{4,} \. eyJ[A-Za-z0-9_\-]{4,} \. [A-Za-z0-9_\-]{4,}
";

/// A PEM private key, header to footer.
///
/// A key with no footer is not left in the clear either: the `BEGIN` line is not
/// the secret, and matching only it would scrub the one part of the block that
/// is safe to keep. The truncated case consumes the PEM alphabet — base64,
/// whitespace and the punctuation a header line uses — and stops at the first
/// character a key body cannot contain. That bound is deliberate rather than
/// tidy: `.*` would run to the end of the text, and on an already-encoded JSON
/// log line it would eat every field after the key and leave something that no
/// longer parses.
const PRIVATE_KEY_BLOCK: &str = concat!(
    r"(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----",
    r"(?:.*?-----END[A-Z ]*PRIVATE KEY-----|[A-Za-z0-9+/=\s:,-]*)",
);

/// The compiled form of every pattern above, in both engines.
struct Patterns {
    url_userinfo: Regex,
    authorization_header: Regex,
    authorization_scheme: Regex,
    credential_parameter: Regex,
    credential_token: Regex,
    private_key_block: Regex,
    byte_url_userinfo: ByteRegex,
    byte_authorization_header: ByteRegex,
    byte_authorization_scheme: ByteRegex,
    byte_credential_parameter: ByteRegex,
    byte_credential_token: ByteRegex,
}

/// Compiled once for the life of the process.
///
/// Every pattern here is a constant this build wrote, so a compile failure is a
/// bug in this file rather than a runtime condition a caller could handle —
/// hence `expect`. It happens at most once, before any redaction runs.
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        url_userinfo: compile(URL_USERINFO),
        authorization_header: compile(AUTHORIZATION_HEADER),
        authorization_scheme: compile(AUTHORIZATION_SCHEME),
        credential_parameter: compile(CREDENTIAL_PARAMETER),
        credential_token: compile(CREDENTIAL_TOKEN),
        private_key_block: compile(PRIVATE_KEY_BLOCK),
        byte_url_userinfo: compile_bytes(URL_USERINFO),
        byte_authorization_header: compile_bytes(AUTHORIZATION_HEADER),
        byte_authorization_scheme: compile_bytes(AUTHORIZATION_SCHEME),
        byte_credential_parameter: compile_bytes(CREDENTIAL_PARAMETER),
        byte_credential_token: compile_bytes(CREDENTIAL_TOKEN),
    })
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("every redaction pattern is a constant this build wrote")
}

fn compile_bytes(pattern: &str) -> ByteRegex {
    ByteRegex::new(pattern).expect("every redaction pattern is a constant this build wrote")
}

/// Whether a `Bearer`-introduced value looks like a credential rather than a word.
///
/// One non-letter is the whole test. Real tokens are base64, hex, or dotted
/// segments and always carry a digit, a dash, a dot or a slash; English words
/// never do.
fn is_credential_shaped(value: &str) -> bool {
    value
        .chars()
        .any(|character| !character.is_ascii_alphabetic())
}

/// Whether an `Authorization` value is a credential rather than an explanation.
///
/// A header's value is a registered scheme and a token, or one opaque token on
/// its own. Prose is neither, and prose is what a policy or an agent refusal
/// looks like: `authorization: denied because the grant no longer covers
/// remote_write` is the reason a call was refused, and replacing it would
/// destroy the one field an operator opened the log for. The rule is aggressive
/// by design — it takes a header's *whole* value — so it needs a way to tell
/// what it is looking at, and this is it.
fn is_authorization_value(value: &str) -> bool {
    let mut words = value.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if AUTHORIZATION_SCHEMES
        .iter()
        .any(|scheme| first.eq_ignore_ascii_case(scheme))
    {
        return true;
    }
    // A scheme-less header carries one opaque token, so anything with a space
    // in it is prose. An all-letter word is a word; `credential_token` and the
    // declared-secret rule are what remain for the rest.
    words.next().is_none() && is_credential_shaped(first)
}

/// Applies every shape rule to `text`, or reports that none of them fired.
///
/// `None` is the common answer and the reason
/// [`Redactor::redact_text`](crate::store::Redactor::redact_text) can hand back
/// a borrow: clean text costs one scan per rule and no allocation.
pub(super) fn redact_shapes(text: &str) -> Option<String> {
    let patterns = patterns();
    let mut current: Option<String> = None;

    take(&mut current, text, |source| {
        patterns
            .private_key_block
            .replace_all(source, PRIVATE_KEY_BLOCK_MARKER)
    });
    take(&mut current, text, |source| {
        patterns
            .url_userinfo
            .replace_all(source, URL_USERINFO_REPLACEMENT)
    });
    take(&mut current, text, |source| {
        patterns
            .authorization_header
            .replace_all(source, |captures: &Captures<'_>| {
                if is_authorization_value(&captures[3]) {
                    format!("{}{}{AUTHORIZATION_MARKER}", &captures[1], &captures[2])
                } else {
                    captures[0].to_owned()
                }
            })
    });
    take(&mut current, text, |source| {
        patterns
            .authorization_scheme
            .replace_all(source, |captures: &Captures<'_>| {
                let scheme = &captures[1];
                let value = &captures[2];
                if is_credential_shaped(value) {
                    format!("{scheme} {AUTHORIZATION_MARKER}")
                } else {
                    captures[0].to_owned()
                }
            })
    });
    take(&mut current, text, |source| {
        patterns
            .credential_parameter
            .replace_all(source, CREDENTIAL_PARAMETER_REPLACEMENT)
    });
    take(&mut current, text, |source| {
        patterns
            .credential_token
            .replace_all(source, CREDENTIAL_TOKEN_MARKER)
    });
    current
}

/// Runs one rule against whichever of `original` or the accumulator is current.
///
/// Written this way rather than as a fold over `Cow`s because each
/// `replace_all` borrows from the string it was handed, and a fold would have to
/// keep every intermediate alive to satisfy that.
///
/// The equality check is what makes redaction *idempotent*, and it is not
/// optional. A rule that matched and then declined — the `Bearer` guard —
/// still returns an owned copy, and a marker left by an earlier pass matches
/// the very rule that wrote it and is replaced by itself. Recording either as a
/// change would make already-clean text report as rewritten, so the caller could
/// no longer borrow and `redact(redact(x))` would allocate forever.
fn take<'a, F>(current: &mut Option<String>, original: &'a str, apply: F)
where
    F: for<'b> FnOnce(&'b str) -> Cow<'b, str>,
{
    let changed = {
        let source: &str = current.as_deref().unwrap_or(original);
        match apply(source) {
            Cow::Owned(rewritten) if rewritten != source => Some(rewritten),
            _ => None,
        }
    };
    if let Some(rewritten) = changed {
        *current = Some(rewritten);
    }
}

/// The byte-stream counterpart, for one line of an artifact.
///
/// Applies every rule [`RedactionRule::covers_streams`] admits. The returned
/// vector is `None` when nothing matched, so an artifact of clean bytes is
/// copied through with no per-line allocation.
pub(super) fn redact_shape_bytes(line: &[u8]) -> Option<Vec<u8>> {
    let patterns = patterns();
    let mut current: Option<Vec<u8>> = None;

    take_bytes(&mut current, line, |source| {
        patterns
            .byte_url_userinfo
            .replace_all(source, URL_USERINFO_REPLACEMENT.as_bytes())
    });
    take_bytes(&mut current, line, |source| {
        patterns.byte_authorization_header.replace_all(
            source,
            |captures: &regex::bytes::Captures<'_>| {
                // Lossy is exact here: the value class is ASCII, so a byte that
                // would decode lossily could not have been matched.
                if is_authorization_value(&String::from_utf8_lossy(&captures[3])) {
                    let mut replacement = captures[1].to_vec();
                    replacement.extend_from_slice(&captures[2]);
                    replacement.extend_from_slice(AUTHORIZATION_MARKER.as_bytes());
                    replacement
                } else {
                    captures[0].to_vec()
                }
            },
        )
    });
    take_bytes(&mut current, line, |source| {
        patterns.byte_authorization_scheme.replace_all(
            source,
            |captures: &regex::bytes::Captures<'_>| {
                // The capture class is `[A-Za-z0-9._~+/=-]`, so asking the bytes
                // directly answers exactly what decoding them would, without an
                // allocation on a path that runs per artifact line.
                let credential_shaped = captures[2].iter().any(|byte| !byte.is_ascii_alphabetic());
                if credential_shaped {
                    let mut replacement = captures[1].to_vec();
                    replacement.push(b' ');
                    replacement.extend_from_slice(AUTHORIZATION_MARKER.as_bytes());
                    replacement
                } else {
                    captures[0].to_vec()
                }
            },
        )
    });
    take_bytes(&mut current, line, |source| {
        patterns
            .byte_credential_parameter
            .replace_all(source, CREDENTIAL_PARAMETER_REPLACEMENT.as_bytes())
    });
    take_bytes(&mut current, line, |source| {
        patterns
            .byte_credential_token
            .replace_all(source, CREDENTIAL_TOKEN_MARKER.as_bytes())
    });
    current
}

fn take_bytes<'a, F>(current: &mut Option<Vec<u8>>, original: &'a [u8], apply: F)
where
    F: for<'b> FnOnce(&'b [u8]) -> Cow<'b, [u8]>,
{
    let changed = {
        let source: &[u8] = current.as_deref().unwrap_or(original);
        match apply(source) {
            Cow::Owned(rewritten) if rewritten != source => Some(rewritten),
            _ => None,
        }
    };
    if let Some(rewritten) = changed {
        *current = Some(rewritten);
    }
}

/// Replaces every occurrence of `needle` in `haystack`, byte for byte.
///
/// The declared-secret rule on the stream side. `str::replace` is unavailable
/// because an artifact is not required to be UTF-8, and a secret is a literal
/// here rather than a pattern, so no engine is involved at all.
pub(super) fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Option<Vec<u8>> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut rewritten: Option<Vec<u8>> = None;
    let mut cursor = 0;
    while cursor + needle.len() <= haystack.len() {
        if &haystack[cursor..cursor + needle.len()] == needle {
            let output = rewritten.get_or_insert_with(|| haystack[..cursor].to_vec());
            output.extend_from_slice(replacement);
            cursor += needle.len();
        } else {
            if let Some(output) = rewritten.as_mut() {
                output.push(haystack[cursor]);
            }
            cursor += 1;
        }
    }
    if let Some(output) = rewritten.as_mut() {
        output.extend_from_slice(&haystack[cursor..]);
    }
    rewritten
}

/// What the declared-secret rule leaves behind, shared by both engines.
pub(super) const fn declared_secret_marker() -> &'static str {
    DECLARED_SECRET_MARKER
}

#[cfg(test)]
mod tests {
    use super::{RedactionRule, redact_shape_bytes, redact_shapes, replace_bytes};

    /// Applies every rule, as `redact_text` does.
    fn redact(text: &str) -> String {
        redact_shapes(text).unwrap_or_else(|| text.to_owned())
    }

    /// Asserts a rule fired and left nothing of the secret behind.
    fn assert_redacted(text: &str, secret: &str, rule: RedactionRule) {
        let redacted = redact(text);
        assert!(
            !redacted.contains(secret),
            "{rule:?} left {secret:?} in {redacted:?}"
        );
        assert!(
            redacted.contains(rule.marker()),
            "{rule:?} should have named itself in {redacted:?}"
        );
    }

    /// Asserts nothing changed at all — the negative half of every rule.
    fn assert_untouched(text: &str) {
        assert_eq!(
            redact_shapes(text),
            None,
            "{text:?} matches no rule and must not be rewritten"
        );
    }

    // -- rule 1: URL userinfo -------------------------------------------------

    #[test]
    fn a_url_password_and_the_username_beside_it_both_go() {
        assert_redacted(
            "fatal: could not fetch https://user:hunter2@example.com/repo.git",
            "hunter2",
            RedactionRule::UrlUserinfo,
        );
        assert_eq!(
            redact("https://user:hunter2@example.com/repo.git"),
            "https://«redacted:url_userinfo»@example.com/repo.git",
            "the host and path are what a run record needs; the userinfo is not"
        );
    }

    #[test]
    fn a_token_used_as_a_url_username_goes_too() {
        assert_redacted(
            "remote: https://ghp_0123456789abcdefghijklmnopqrstuvwxyzAB@github.com/o/r",
            "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
            RedactionRule::UrlUserinfo,
        );
    }

    #[test]
    fn a_url_with_no_userinfo_and_an_address_that_is_not_a_url_are_left_alone() {
        assert_untouched("cloning https://example.com/repo.git into ./repo");
        assert_untouched("mail the maintainer at user@example.com");
        assert_untouched("git@github.com:owner/repo.git");
        assert_untouched("see https://example.com/owner@repo/blob/main");
    }

    #[test]
    fn a_rule_never_reaches_across_the_structure_of_the_line_it_is_scrubbing() {
        // The shape that made a negated character class wrong: the log writer
        // redacts an *already encoded* JSON line, and a class admitting `"`
        // matched from the scheme in one field to the `@` in the next, leaving
        // something that no longer parsed. A record redaction destroyed is worse
        // than one it failed to scrub.
        let encoded = r#"{"remote":"https://example.com","author":"a@b.com"}"#;
        assert_untouched(encoded);
        assert!(serde_json::from_str::<serde_json::Value>(&redact(encoded)).is_ok());

        let leaking = r#"{"remote":"https://u:hunter2@example.com","author":"a@b.com"}"#;
        let redacted = redact(leaking);
        assert!(!redacted.contains("hunter2"));
        let parsed: serde_json::Value = serde_json::from_str(&redacted).expect(&redacted);
        assert_eq!(parsed["author"], "a@b.com", "only the leaking field moved");
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_value_early_and_leave_the_secret_behind() {
        // The shape a failure message takes once the formatter has encoded it:
        // the tool quoted a command line, so the log line holds `\"` rather than
        // `"`. A value class that admitted the backslash matched *it* and
        // stopped, leaving the credential in the clear and the line unparseable
        // — the worst of both halves of this module's job.
        let encoded = r#"{"failure":"git: remote: password=\"hunter2\" rejected","run_id":"r-1"}"#;

        let redacted = redact(encoded);

        assert!(!redacted.contains("hunter2"), "{redacted}");
        let parsed: serde_json::Value = serde_json::from_str(&redacted).expect(&redacted);
        assert_eq!(parsed["run_id"], "r-1");

        let header = r#"{"failure":"sent Authorization: \"Bearer abc.123\" upstream"}"#;
        let redacted = redact(header);
        assert!(!redacted.contains("abc.123"), "{redacted}");
        serde_json::from_str::<serde_json::Value>(&redacted).expect(&redacted);
    }

    #[test]
    fn a_credential_key_whose_value_is_an_object_loses_no_structure() {
        // The counterpart hazard: `"token": {…}` in an encoded line. A value
        // class that admitted `{` would replace the opening brace and leave a
        // line nothing could parse — while the strings inside the object have
        // already been redacted by value on their way into the store.
        let encoded = r#"{"token":{"kind":"bearer"},"run_id":"r-1"}"#;

        let redacted = redact(encoded);

        let parsed: serde_json::Value = serde_json::from_str(&redacted).expect(&redacted);
        assert_eq!(parsed["run_id"], "r-1");
        assert_eq!(parsed["token"]["kind"], "bearer");

        let header = r#"{"authorization":{"scheme":"bearer"},"run_id":"r-2"}"#;
        let parsed: serde_json::Value =
            serde_json::from_str(&redact(header)).expect("the header rule stops at the brace too");
        assert_eq!(parsed["run_id"], "r-2");
    }

    #[test]
    fn a_truncated_private_key_does_not_swallow_the_rest_of_an_encoded_line() {
        let encoded =
            r#"{"note":"-----BEGIN RSA PRIVATE KEY-----MIIEowIBAAKCAQEA0123","run_id":"r-1"}"#;

        let redacted = redact(encoded);

        assert!(!redacted.contains("MIIEowIBAAKCAQEA0123"), "{redacted}");
        let parsed: serde_json::Value = serde_json::from_str(&redacted).expect(&redacted);
        assert_eq!(
            parsed["run_id"], "r-1",
            "a bounded fallback is what keeps the fields after a key readable"
        );
    }

    // -- rule 2: authorization and credential parameters ----------------------

    #[test]
    fn an_authorization_header_loses_its_whole_value() {
        assert_redacted(
            "> Authorization: Bearer abcdef.0123456789",
            "abcdef.0123456789",
            RedactionRule::Authorization,
        );
        assert_redacted(
            r#"{"authorization": "Basic dXNlcjpodW50ZXIy"}"#,
            "dXNlcjpodW50ZXIy",
            RedactionRule::Authorization,
        );
    }

    #[test]
    fn a_bare_bearer_credential_is_recognized_without_a_header_name() {
        assert_redacted(
            "retrying with bearer eyJhbGciOi.payload",
            "eyJhbGciOi.payload",
            RedactionRule::Authorization,
        );
    }

    #[test]
    fn english_that_happens_to_begin_with_bearer_is_left_alone() {
        assert_untouched("the bearer of bad news");
        assert_untouched("basic authentication was disabled");
        assert_untouched("authorization:");
    }

    #[test]
    fn credential_parameters_are_scrubbed_in_urls_json_and_env_lines() {
        assert_redacted(
            "GET /api?access_token=s3cr3t-value&page=2",
            "s3cr3t-value",
            RedactionRule::CredentialParameter,
        );
        assert_redacted(
            r#"{"api_key": "AAAABBBBCCCC", "region": "eu"}"#,
            "AAAABBBBCCCC",
            RedactionRule::CredentialParameter,
        );
        assert_redacted(
            "PGPASSWORD=letmein psql -h db",
            "letmein",
            RedactionRule::CredentialParameter,
        );
    }

    #[test]
    fn the_rest_of_the_line_survives_a_credential_parameter() {
        assert_eq!(
            redact("GET /api?access_token=s3cr3t-value&page=2"),
            "GET /api?access_token=«redacted:credential_parameter»&page=2"
        );
    }

    #[test]
    fn ordinary_configuration_is_not_a_credential_parameter() {
        assert_untouched("key = value");
        assert_untouched("id=42");
        assert_untouched("tokens = 5");
        assert_untouched("the password was rotated");
        assert_untouched("secretariat=racehorse");
    }

    #[test]
    fn the_working_directory_is_not_a_credential() {
        // `pwd` was on the key list and `PWD=`/`OLDPWD=` are in every
        // environment dump a tool prints. The working directory is one of the
        // first things anybody needs to reconstruct a failed run, so scrubbing
        // it destroys far more than it protects — the same reasoning that keeps
        // `key=` and `id=` off the list.
        assert_untouched("PWD=/home/taiye/project");
        assert_untouched("OLDPWD=/tmp");
        assert_untouched("cd $PWD && cargo test");
    }

    #[test]
    fn a_refusal_explained_after_a_colon_keeps_its_explanation() {
        // The header rule takes a value *whole*, so it has to be able to tell a
        // credential from prose. A denial reason is the one field an operator
        // opened the log for; replacing it would be the rule doing the opposite
        // of its job.
        assert_untouched("authorization: denied because the grant no longer covers remote_write");
        assert_untouched("authorization = pending review by the workspace owner");
        assert_untouched("proxy-authorization: not required for this endpoint");
    }

    // -- rule 3: published token prefixes -------------------------------------

    #[test]
    fn every_published_prefix_matches_its_own_shape() {
        let cases = [
            "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
            "gho_0123456789abcdefghijklmnopqrstuvwxyzAB",
            "github_pat_0123456789abcdefghijkl_0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklm",
            "glpat-0123456789abcdefghij",
            "xoxb-0123456789-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA0123456789abcdefghijklmnopqrstuv",
            "sk-ant-api03-0123456789abcdefghij",
            "sk_live_0123456789abcdefghij",
            "npm_0123456789abcdefghijklmnopqrstuvwxyzAB",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ];
        for secret in cases {
            assert_redacted(
                &format!("the tool printed {secret} to stdout"),
                secret,
                RedactionRule::CredentialToken,
            );
        }
    }

    #[test]
    fn ordinary_base64_and_hashes_are_not_tokens() {
        assert_untouched("SGVsbG8sIHdvcmxkISBUaGlzIGlzIG9yZGluYXJ5IGJhc2U2NC4=");
        assert_untouched("commit c3abd0463f2b0f1f4a3d5e6c7b8a9d0e1f2a3b4c");
        assert_untouched("AKIASHORT");
        assert_untouched("ghp_tooshort");
    }

    // -- private key blocks ---------------------------------------------------

    #[test]
    fn a_pem_private_key_goes_from_begin_to_end() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNza\nAAAABBBB\n-----END OPENSSH PRIVATE KEY-----";
        assert_redacted(
            &format!("wrote the key:\n{key}\nand carried on"),
            "b3BlbnNza",
            RedactionRule::PrivateKeyBlock,
        );
        assert_eq!(
            redact(&format!("wrote the key:\n{key}\nand carried on")),
            "wrote the key:\n«redacted:private_key_block»\nand carried on"
        );
    }

    #[test]
    fn a_key_with_no_footer_still_does_not_survive() {
        let truncated = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0123";
        assert_redacted(
            truncated,
            "MIIEowIBAAKCAQEA0123",
            RedactionRule::PrivateKeyBlock,
        );
    }

    // -- markers --------------------------------------------------------------

    #[test]
    fn a_marker_names_its_rule_and_echoes_none_of_the_match() {
        for rule in RedactionRule::all() {
            let marker = rule.marker();
            assert_eq!(marker, format!("«redacted:{}»", rule.name()));
        }
    }

    #[test]
    fn redaction_is_idempotent_so_a_second_pass_changes_nothing() {
        let once = redact("https://user:hunter2@example.com?token=abcdefgh");
        assert_eq!(
            redact_shapes(&once),
            None,
            "a marker must not itself look like a secret: {once}"
        );
    }

    // -- the byte engine ------------------------------------------------------

    #[test]
    fn the_byte_engine_agrees_with_the_string_engine_on_text() {
        let line = "fatal: https://user:hunter2@example.com and Authorization: Bearer abc.123";
        let bytes = redact_shape_bytes(line.as_bytes()).expect("both rules fire");
        let text = redact_shapes(line).expect("both rules fire");

        assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    #[test]
    fn binary_bytes_that_match_nothing_are_left_exactly_as_they_arrived() {
        let binary: Vec<u8> = (0u8..=255).collect();

        assert_eq!(redact_shape_bytes(&binary), None);
    }

    #[test]
    fn a_literal_replacement_walks_a_byte_slice_without_decoding_it() {
        let haystack = b"\xff\xfeSECRET\xff between SECRET markers";

        let rewritten = replace_bytes(haystack, b"SECRET", b"[x]").expect("two occurrences");

        assert_eq!(rewritten, b"\xff\xfe[x]\xff between [x] markers".to_vec());
        assert_eq!(replace_bytes(haystack, b"ABSENT", b"[x]"), None);
        assert_eq!(replace_bytes(haystack, b"", b"[x]"), None);
    }

    #[test]
    fn the_stream_rules_are_every_rule_but_the_one_that_spans_lines() {
        let streaming: Vec<_> = RedactionRule::all()
            .iter()
            .copied()
            .filter(|rule| rule.covers_streams())
            .collect();

        assert!(!streaming.contains(&RedactionRule::PrivateKeyBlock));
        assert_eq!(streaming.len(), RedactionRule::all().len() - 1);
    }
}
