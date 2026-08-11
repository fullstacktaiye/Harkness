//! How a file is treated once the context engine has found it.
//!
//! The vocabulary lives here; the heuristics that assign it do not. Deciding
//! that a path is generated, vendored, or oversized is [#112]'s job, and it
//! wants a fixed set of answers to return rather than a set it can extend as it
//! goes.
//!
//! [#112]: https://github.com/fullstacktaiye/harkness/issues/112

use serde::{Deserialize, Serialize};

/// What kind of file a path holds, for retrieval and exclusion decisions.
///
/// # Forward compatibility
///
/// The enum is [`non_exhaustive`], so a later release may add a class and
/// downstream crates must keep a wildcard arm. Deserialization is deliberately
/// *not* forgiving in the same way: a spelling this build does not define is a
/// hard error, never a silent coercion to [`FileClass::UnknownText`]. The
/// catalog takes the same position on same-version unknown fields, and for the
/// same reason — a file quietly reclassified from `secret_sensitive` to
/// something benign is an exclusion that stops happening without anyone being
/// told. A build that meets a class it does not know is out of date, and saying
/// so is the safe answer.
///
/// [`non_exhaustive`]: https://doc.rust-lang.org/reference/attributes/type_system.html
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileClass {
    /// Program text the user wrote and the model may be asked to change.
    Source,
    /// Program text whose purpose is to exercise other program text.
    TestCode,
    /// Settings consumed by a tool or a runtime.
    Configuration,
    /// Prose written for people: guides, references, changelogs.
    Documentation,
    /// Prose written for an agent: the discovered instruction set.
    Instruction,
    /// A manifest that declares a build's dependencies and targets.
    BuildManifest,
    /// Output of a generator, reproducible from something else in the tree.
    Generated,
    /// Third-party code checked into the tree rather than depended on.
    Vendor,
    /// A resolved dependency graph, large and rarely worth reading.
    Lockfile,
    /// Content with no useful text form.
    Binary,
    /// Content that matches a secret-bearing rule and must not be retrieved.
    SecretSensitive,
    /// Text past the size at which retrieval stops being worthwhile.
    Oversized,
    /// Text whose kind could not be determined.
    UnknownText,
    /// Content that claims to be text but cannot be decoded as any encoding
    /// Harkness reads.
    UnsupportedEncoding,
}

impl FileClass {
    /// Every file class in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Source,
        Self::TestCode,
        Self::Configuration,
        Self::Documentation,
        Self::Instruction,
        Self::BuildManifest,
        Self::Generated,
        Self::Vendor,
        Self::Lockfile,
        Self::Binary,
        Self::SecretSensitive,
        Self::Oversized,
        Self::UnknownText,
        Self::UnsupportedEncoding,
    ];

    /// Returns the stable persisted spelling of this class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::TestCode => "test_code",
            Self::Configuration => "configuration",
            Self::Documentation => "documentation",
            Self::Instruction => "instruction",
            Self::BuildManifest => "build_manifest",
            Self::Generated => "generated",
            Self::Vendor => "vendor",
            Self::Lockfile => "lockfile",
            Self::Binary => "binary",
            Self::SecretSensitive => "secret_sensitive",
            Self::Oversized => "oversized",
            Self::UnknownText => "unknown_text",
            Self::UnsupportedEncoding => "unsupported_encoding",
        }
    }

    /// Whether a file of this class may ever be shown to a model.
    ///
    /// Advisory: the exclusion itself is enforced where retrieval happens. The
    /// answer lives beside the vocabulary so two components cannot disagree
    /// about what `secret_sensitive` means.
    #[must_use]
    pub const fn is_retrievable(self) -> bool {
        !matches!(
            self,
            Self::SecretSensitive | Self::Binary | Self::UnsupportedEncoding
        )
    }
}

impl std::fmt::Display for FileClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::FileClass;

    #[test]
    fn there_are_exactly_fourteen_documented_classes() {
        assert_eq!(FileClass::ALL.len(), 14);
        let mut spellings = FileClass::ALL
            .iter()
            .map(|class| class.as_str())
            .collect::<Vec<_>>();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), 14, "two classes share a spelling");
    }

    #[test]
    fn every_class_serializes_as_its_snake_case_spelling() {
        for class in FileClass::ALL {
            let json = serde_json::to_string(class).unwrap();
            assert_eq!(json, format!("\"{}\"", class.as_str()));
            assert_eq!(&serde_json::from_str::<FileClass>(&json).unwrap(), class);
            assert_eq!(class.to_string(), class.as_str());
        }
    }

    #[test]
    fn an_unknown_class_fails_rather_than_coercing() {
        for spelling in ["\"Source\"", "\"secret\"", "\"executable_bit\"", "\"\""] {
            assert!(
                serde_json::from_str::<FileClass>(spelling).is_err(),
                "accepted {spelling}"
            );
        }
    }

    #[test]
    fn the_classes_that_must_never_be_retrieved_are_named() {
        let blocked = FileClass::ALL
            .iter()
            .filter(|class| !class.is_retrievable())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            blocked,
            [
                FileClass::Binary,
                FileClass::SecretSensitive,
                FileClass::UnsupportedEncoding
            ]
        );
    }
}
