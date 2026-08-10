use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

define_id!(
    /// A stable identifier for one user task.
    TaskId
);
define_id!(
    /// A stable identifier for one attempt to execute a task.
    RunId
);
define_id!(
    /// A stable identifier for one ordered step in a run.
    StepId
);
define_id!(
    /// A stable identifier for one requested tool invocation.
    ToolCallId
);

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde::{Serialize, de::DeserializeOwned};

    use super::{RunId, StepId, TaskId, ToolCallId};

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
    }
}
