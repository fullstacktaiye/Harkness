use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Declares a UUID newtype with the identifier contract every Harkness
/// identity shares: `Copy`, total ordering, transparent serde, canonical
/// hyphenated `Display`, and a `Default` that generates a fresh value.
///
/// Shared with [`crate::integration`] so an external-subject identifier is the
/// same kind of thing as a [`TaskId`] rather than a second convention. The
/// expansion names `Uuid`, `Serialize`, and `Deserialize` unqualified, so a
/// call site imports all three.
macro_rules! define_id {
    ($(#[$metadata:meta])* $name:ident) => {
        $(#[$metadata])*
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            /// Generates a fresh random identifier; there is no empty ID value.
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            /// Accepts every UUID spelling supported by [`Uuid::parse_str`].
            /// Display and serialization always return canonical hyphenated form.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

pub(crate) use define_id;

define_id!(
    /// A stable identifier for one user task.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    TaskId
);
define_id!(
    /// A stable identifier for one attempt to execute a task.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    RunId
);
define_id!(
    /// A stable identifier for one ordered step in a run.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    StepId
);
define_id!(
    /// A stable identifier for one requested tool invocation.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ToolCallId
);
define_id!(
    /// A stable identifier for one durable approval request.
    ///
    /// It names the question a human was asked, the answer that resolved it, and
    /// the grant that answer produced, so a timeline entry, a front end's prompt,
    /// and a matched grant all refer to the same record rather than to three
    /// separately-derived descriptions of one pause.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ApprovalId
);
define_id!(
    /// A stable identifier for one stored artifact.
    ///
    /// An artifact is content too large to live in a row: a build log, a diff,
    /// an overflowed event payload. The identity names both the metadata row and
    /// the file holding the bytes, so the two can never drift apart.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ArtifactId
);

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde::{Serialize, de::DeserializeOwned};

    use super::{ApprovalId, ArtifactId, RunId, StepId, TaskId, ToolCallId};

    const FIXTURE_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn assert_id_contract<T>()
    where
        T: Copy
            + std::fmt::Debug
            + Eq
            + FromStr<Err = uuid::Error>
            + Serialize
            + DeserializeOwned
            + std::fmt::Display,
    {
        let id = T::from_str(FIXTURE_ID).unwrap();
        assert_eq!(id.to_string(), FIXTURE_ID);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{FIXTURE_ID}\""));
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), id);
    }

    #[test]
    fn ids_parse_display_and_serde_round_trip_like_project_id() {
        assert_id_contract::<TaskId>();
        assert_id_contract::<RunId>();
        assert_id_contract::<StepId>();
        assert_id_contract::<ToolCallId>();
        assert_id_contract::<ArtifactId>();
        assert_id_contract::<ApprovalId>();
    }

    #[test]
    fn default_generates_a_fresh_random_identity() {
        assert_ne!(RunId::default(), RunId::default());
    }

    #[test]
    fn accepted_uuid_spellings_canonicalize_on_display() {
        for spelling in [
            "123e4567e89b42d3a456426614174000",
            "{123e4567-e89b-42d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-42d3-a456-426614174000",
        ] {
            assert_eq!(RunId::from_str(spelling).unwrap().to_string(), FIXTURE_ID);
        }
    }
}
