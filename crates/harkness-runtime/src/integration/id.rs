use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::id::define_id;

define_id!(
    /// A stable identifier for one registered ACP agent.
    ///
    /// The identity is the registration, not the executable: replacing the
    /// binary keeps this value and changes the
    /// [`IdentityBasis`](super::IdentityBasis) a grant was bound to, which is
    /// what makes the swap visible instead of silent.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ExternalAgentId
);
define_id!(
    /// A stable identifier for one configured MCP server.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    McpServerId
);
define_id!(
    /// A stable identifier for one tool record published by an MCP server.
    ///
    /// A tool is a subject in its own right, separate from the server that
    /// serves it, because a server that reshapes a tool's schema after being
    /// trusted has changed something the user never agreed to.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    McpToolRef
);
define_id!(
    /// A stable identifier for one workflow recipe.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    RecipeId
);
define_id!(
    /// A stable identifier for one forge account.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ForgeAccountId
);
define_id!(
    /// A stable identifier for one forge repository.
    ///
    /// The canonical remote — `github.com/{owner}/{repo}` — is the repository's
    /// *identity basis*, not its identifier: a remote repointed at another
    /// repository keeps this value and invalidates the grant.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ForgeRepoRef
);

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde::{Serialize, de::DeserializeOwned};

    use super::{ExternalAgentId, ForgeAccountId, ForgeRepoRef, McpServerId, McpToolRef, RecipeId};

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
    fn integration_ids_parse_display_and_serde_round_trip_like_task_id() {
        assert_id_contract::<ExternalAgentId>();
        assert_id_contract::<McpServerId>();
        assert_id_contract::<McpToolRef>();
        assert_id_contract::<RecipeId>();
        assert_id_contract::<ForgeAccountId>();
        assert_id_contract::<ForgeRepoRef>();
    }

    #[test]
    fn default_generates_a_fresh_random_identity() {
        assert_ne!(McpServerId::default(), McpServerId::default());
        assert_ne!(RecipeId::default(), RecipeId::default());
    }

    #[test]
    fn accepted_uuid_spellings_canonicalize_on_display() {
        for spelling in [
            "123e4567e89b42d3a456426614174000",
            "{123e4567-e89b-42d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-42d3-a456-426614174000",
        ] {
            assert_eq!(
                ExternalAgentId::from_str(spelling).unwrap().to_string(),
                FIXTURE_ID
            );
        }
    }
}
