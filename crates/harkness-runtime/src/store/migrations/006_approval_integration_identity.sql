-- Migration 6: bind external-integration approvals to observed subject identity.
--
-- NULL preserves every v0.3/v0.4 approval. For a v0.5 operation the relevant
-- hash is populated before the question is shown, then compared as part of the
-- grant on every scope. The other two remain NULL; absence participates in the
-- exact identity comparison just as a present digest does.
ALTER TABLE approvals ADD COLUMN agent_executable_sha256 TEXT;
ALTER TABLE approvals ADD COLUMN mcp_tool_schema_fingerprint TEXT;
ALTER TABLE approvals ADD COLUMN recipe_content_hash TEXT;
