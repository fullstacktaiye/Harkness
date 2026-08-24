//! Values this process knows are secret, and must never write down.
//!
//! The pattern rules beside this module recognize secrets by *shape* —
//! a URL's userinfo, an `Authorization` header, a token whose prefix its issuer
//! publishes. That covers credentials Harkness never handled, which is most of
//! them, and it cannot cover a secret with no shape at all: a passphrase, an
//! internal service token, a base64 blob from a private issuer. Nothing about
//! those strings distinguishes them from ordinary output.
//!
//! What distinguishes them is that Harkness *handed them over*. A process tool
//! copies an allowlisted environment variable into a child, and from that moment
//! the value can come back in the child's stdout, in its stderr, in an error
//! message quoting a command line, or in a result payload. Declaring it here is
//! the only way a later rule can recognize it, so the declaration happens at the
//! spawn — see [`ToolProcess`](crate::tool::ToolProcess) — rather than being
//! configured somewhere a contributor has to remember to update.
//!
//! # Why a registry and not a field on the redactor
//!
//! A [`Redactor`](crate::store::Redactor) is installed when a store is opened
//! and is never swapped, because a redactor that changed underneath a running
//! write would be a race. Declared secrets have the opposite lifetime: they are
//! discovered as work runs, and the set only ever *grows*. A registry the
//! redactor holds by handle keeps both properties — the redactor is fixed, and
//! what it knows can only ever increase, so no write can be less redacted than
//! it would have been a moment earlier.
//!
//! # Minimum length, and why a limit exists at all
//!
//! Replacing every occurrence of a three-character value would corrupt far more
//! records than it protects: `abc` appears in paths, identifiers and prose, and
//! an audit trail scrubbed of it says nothing. Declaration therefore refuses
//! anything shorter than [`MIN_DECLARED_SECRET_BYTES`] and says so, rather than
//! silently accepting a rule that would eat the log. The consequence is stated
//! plainly in `docs/observability.md`: a genuinely short secret is not covered
//! by this rule, and the shape rules are what remain.
//!
//! # The value is held, and that is the point
//!
//! Exact-match replacement needs the bytes. They live in a value type
//! which overwrites its own buffer before it is freed, and which has no
//! `Display`, no `Debug` that prints it, and no accessor that hands it out. The
//! only thing that can read one is the replacement loop in this module.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::sync::atomic::{Ordering, compiler_fence};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// Shortest value [`SecretRegistry::declare`] will accept.
///
/// Six bytes is not a claim about entropy. It is the point below which
/// exact-match replacement stops protecting a record and starts destroying it,
/// because a shorter value is likely to be a substring of something innocent.
pub const MIN_DECLARED_SECRET_BYTES: usize = 6;

/// What a declaration did.
///
/// Returned rather than swallowed so a caller can log the refusal: a secret
/// nobody redacts is worth a line in the diagnostic log, and a caller that
/// cannot tell acceptance from refusal would have no way to write one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Declared {
    /// The value is now redacted wherever it appears.
    Accepted,
    /// The registry already held this value; nothing changed.
    AlreadyKnown,
    /// Shorter than [`MIN_DECLARED_SECRET_BYTES`], so it was not accepted.
    TooShort,
}

/// A secret value held only for as long as it must be, and overwritten after.
///
/// There is deliberately no way to read one out. `Debug` prints the length and
/// nothing else, so a registry can appear in a struct that derives `Debug`
/// without the derive becoming the leak this whole module exists to prevent.
struct SecretValue(String);

impl SecretValue {
    fn new(value: &str) -> Self {
        Self(value.to_owned())
    }

    fn matches(&self, candidate: &str) -> bool {
        self.0 == candidate
    }

    fn as_needle(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "SecretValue({} bytes)", self.0.len())
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        // `String::clear` sets the length and frees nothing, so the bytes would
        // still be sitting in the allocation when the allocator hands it to the
        // next caller. Volatile writes are what stop the optimizer from
        // removing stores to memory it can prove is about to die.
        //
        // SAFETY: every byte written is `0`, which is valid UTF-8 on its own and
        // in any position, so the `String` remains well formed for the moment
        // between this loop and the deallocation that follows it.
        unsafe {
            for byte in self.0.as_bytes_mut() {
                std::ptr::write_volatile(byte, 0);
            }
        }
        compiler_fence(Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
struct Secrets {
    values: Vec<SecretValue>,
}

/// The append-only set of values this process must never write down.
///
/// Cloning shares one set: a registry is a handle, so a redactor built from one
/// sees every later declaration. That is the whole reason it exists.
#[derive(Clone, Debug, Default)]
pub struct SecretRegistry {
    inner: Arc<RwLock<Secrets>>,
}

impl SecretRegistry {
    /// An empty registry that shares nothing with any other.
    ///
    /// What a test uses. Production goes through [`SecretRegistry::process`], so
    /// a secret declared by a tool reaches the redactor a store was opened with
    /// even though the two were built by unrelated code.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The one registry every standard redactor consults.
    ///
    /// Process-wide because the fact it records is process-wide: the value came
    /// out of *this* process's environment and was handed to a child *this*
    /// process spawned. Scoping it to a store would mean a second store in the
    /// same process wrote down a secret the first one knew about.
    #[must_use]
    pub fn process() -> Self {
        static PROCESS: OnceLock<SecretRegistry> = OnceLock::new();
        PROCESS.get_or_init(SecretRegistry::new).clone()
    }

    /// Records `value` as a secret, if it is long enough to be one safely.
    ///
    /// Declaring the same value twice is not an error and costs one comparison
    /// per known secret; the set is small by construction, because it holds one
    /// entry per sensitive environment variable a tool was actually granted.
    pub fn declare(&self, value: &str) -> Declared {
        if value.len() < MIN_DECLARED_SECRET_BYTES {
            return Declared::TooShort;
        }
        let mut secrets = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if secrets.values.iter().any(|known| known.matches(value)) {
            return Declared::AlreadyKnown;
        }
        secrets.values.push(SecretValue::new(value));
        Declared::Accepted
    }

    /// How many values are known. Useful only for diagnostics and tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values
            .len()
    }

    /// Whether anything has been declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replaces every declared value in `text`, or reports that none appeared.
    ///
    /// `None` means "nothing to do", which is the overwhelmingly common answer
    /// and the reason [`redact_text`](crate::store::Redactor::redact_text) can
    /// keep borrowing.
    pub(super) fn redact(&self, text: &str, marker: &str) -> Option<String> {
        let secrets = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        if secrets.values.is_empty() {
            return None;
        }
        let mut rewritten: Option<String> = None;
        for secret in &secrets.values {
            let needle = secret.as_needle();
            let current = rewritten.as_deref().unwrap_or(text);
            if current.contains(needle) {
                rewritten = Some(current.replace(needle, marker));
            }
        }
        rewritten
    }

    /// The same rule against raw bytes, for an artifact stream.
    ///
    /// A declared secret is a literal rather than a pattern, so no engine is
    /// involved and the artifact never has to be decoded — which is what keeps
    /// a binary artifact byte-identical while a credential written into it in
    /// plain text is still found.
    pub(super) fn redact_bytes(&self, bytes: &[u8], marker: &[u8]) -> Option<Vec<u8>> {
        let secrets = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        if secrets.values.is_empty() {
            return None;
        }
        let mut rewritten: Option<Vec<u8>> = None;
        for secret in &secrets.values {
            let needle = secret.as_needle().as_bytes();
            let current: &[u8] = rewritten.as_deref().unwrap_or(bytes);
            if let Some(next) = super::rules::replace_bytes(current, needle, marker) {
                rewritten = Some(next);
            }
        }
        rewritten
    }
}

/// Environment-variable names whose *values* are treated as secrets.
///
/// Substring matching on an upper-cased, separator-stripped name, because the
/// shapes in the wild are endless — `GITHUB_TOKEN`, `npm_config_authToken`,
/// `MY_APP_API_KEY` — and an exhaustive list would be out of date the day it was
/// written.
///
/// Every fragment here had to earn its place against the names a desktop session
/// actually sets. `AUTH` is absent because `GIT_AUTHOR_NAME` contains it and a
/// committer's name is not a credential; `SESSION` is absent because
/// `XDG_SESSION_TYPE` contains it and redacting `wayland` from every record
/// would be absurd; `CERT` is absent because `SSL_CERT_FILE` is a path. Each of
/// those would have declared a value that appears constantly in ordinary output,
/// which is the failure mode this list exists to avoid.
const SENSITIVE_NAME_FRAGMENTS: &[&str] = &[
    "ACCESSKEY",
    "APIKEY",
    "AUTHTOKEN",
    "COOKIE",
    "CREDENTIAL",
    "PASSPHRASE",
    "PASSWD",
    "PASSWORD",
    "PRIVATEKEY",
    "SECRET",
    "SESSIONTOKEN",
    "TOKEN",
];

/// Names that match a fragment above and are nevertheless not secrets.
///
/// Each of these is a *path to a program or a socket*, not a credential. The
/// runner deliberately preserves the askpass helpers so Harkness never handles
/// the secret itself (`harkness-git`'s `runner`), and redacting the helper's
/// path would scrub the one field that says which helper ran while protecting
/// nothing at all.
const NOT_SECRET_NAMES: &[&str] = &[
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_AUTH_SOCK",
    "SUDO_ASKPASS",
];

/// Whether the value of an environment variable called `name` is a secret.
///
/// Case-insensitive, because `npm_config_authToken` and `NPM_CONFIG_AUTHTOKEN`
/// name the same thing to everything except a byte comparison.
#[must_use]
pub fn is_sensitive_environment_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        // A name that is not UTF-8 cannot be compared against these fragments
        // at all. Treating it as insensitive is the honest answer: nothing here
        // learned anything about it, and pretending otherwise would be worse.
        return false;
    };
    let upper = name.to_ascii_uppercase();
    if NOT_SECRET_NAMES.contains(&upper.as_str()) {
        return false;
    }
    // Separators are stripped so `API_KEY`, `API-KEY` and `apiKey` are one name
    // to this check, which is what lets a single fragment list cover the
    // spellings real tools ship.
    let squashed: String = upper
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .collect();
    SENSITIVE_NAME_FRAGMENTS
        .iter()
        .any(|fragment| squashed.contains(fragment))
}

/// Declares the sensitive values of an allowlisted environment.
///
/// Returns the names whose values were accepted, so the caller can record *that*
/// a secret was declared without recording *which value*. A name is safe to log;
/// it is the value this module exists to keep out of the record.
pub fn declare_environment_secrets<'a>(
    registry: &SecretRegistry,
    environment: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
) -> BTreeSet<String> {
    let mut declared = BTreeSet::new();
    for (name, value) in environment {
        if !is_sensitive_environment_name(name) {
            continue;
        }
        let Some(value) = value.to_str() else {
            continue;
        };
        if registry.declare(value) != Declared::TooShort {
            declared.insert(name.to_string_lossy().into_owned());
        }
    }
    declared
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{
        Declared, MIN_DECLARED_SECRET_BYTES, SecretRegistry, declare_environment_secrets,
        is_sensitive_environment_name,
    };

    fn name(text: &str) -> OsString {
        OsString::from(text)
    }

    #[test]
    fn a_declared_value_is_replaced_wherever_it_appears() {
        let registry = SecretRegistry::new();
        assert_eq!(registry.declare("s3kr1t-value"), Declared::Accepted);

        let redacted = registry
            .redact("ran with s3kr1t-value twice: s3kr1t-value", "[gone]")
            .expect("the value appears");

        assert_eq!(redacted, "ran with [gone] twice: [gone]");
    }

    #[test]
    fn text_without_a_declared_value_is_left_for_the_caller_to_borrow() {
        let registry = SecretRegistry::new();
        registry.declare("s3kr1t-value");

        assert!(registry.redact("nothing to see", "[gone]").is_none());
    }

    #[test]
    fn an_empty_registry_short_circuits() {
        assert!(
            SecretRegistry::new()
                .redact("anything at all", "[gone]")
                .is_none()
        );
    }

    #[test]
    fn a_short_value_is_refused_rather_than_allowed_to_eat_the_log() {
        let registry = SecretRegistry::new();
        let short = "a".repeat(MIN_DECLARED_SECRET_BYTES - 1);

        assert_eq!(registry.declare(&short), Declared::TooShort);
        assert!(registry.is_empty());
        assert!(registry.redact("aaaaa in a path", "[gone]").is_none());
    }

    #[test]
    fn declaring_the_same_value_twice_changes_nothing() {
        let registry = SecretRegistry::new();

        assert_eq!(registry.declare("repeated-secret"), Declared::Accepted);
        assert_eq!(registry.declare("repeated-secret"), Declared::AlreadyKnown);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_handle_shares_one_set_so_a_later_declaration_reaches_an_earlier_redactor() {
        let registry = SecretRegistry::new();
        let handle = registry.clone();

        handle.declare("declared-later");

        assert_eq!(
            registry.redact("saw declared-later", "[gone]").as_deref(),
            Some("saw [gone]")
        );
    }

    #[test]
    fn sensitive_names_cover_the_spellings_real_tools_ship() {
        for sensitive in [
            "GITHUB_TOKEN",
            "npm_config_authToken",
            "MY_APP_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "DATABASE_PASSWORD",
            "SESSION-COOKIE",
            "TLS_PRIVATE_KEY",
        ] {
            assert!(
                is_sensitive_environment_name(&name(sensitive)),
                "{sensitive} names a credential"
            );
        }
    }

    #[test]
    fn a_helper_program_path_is_not_a_credential() {
        for benign in [
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "SSH_AUTH_SOCK",
            "GIT_AUTHOR_NAME",
            "XDG_SESSION_TYPE",
            "SSL_CERT_FILE",
            "PATH",
            "HOME",
            "LANG",
            "CARGO_TERM_COLOR",
        ] {
            assert!(
                !is_sensitive_environment_name(&name(benign)),
                "{benign} is a path or a preference, and redacting it protects nothing"
            );
        }
    }

    #[test]
    fn only_sensitive_names_are_declared_and_only_their_names_come_back() {
        let registry = SecretRegistry::new();
        let path = name("PATH");
        let path_value = name("/usr/bin:/bin");
        let token = name("GITHUB_TOKEN");
        let token_value = name("a-token-with-no-shape-at-all");

        let declared = declare_environment_secrets(
            &registry,
            [
                (path.as_os_str(), path_value.as_os_str()),
                (token.as_os_str(), token_value.as_os_str()),
            ],
        );

        assert_eq!(
            declared.into_iter().collect::<Vec<_>>(),
            vec!["GITHUB_TOKEN".to_owned()],
            "the name is reportable; the value never is"
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.redact("/usr/bin:/bin", "[gone]").is_none());
    }

    #[test]
    fn a_debug_rendering_of_the_registry_never_shows_a_value() {
        let registry = SecretRegistry::new();
        registry.declare("do-not-print-me");

        let rendered = format!("{registry:?}");

        assert!(
            !rendered.contains("do-not-print-me"),
            "a derive on a struct holding a registry must not become the leak: {rendered}"
        );
    }
}
