-- Migration 4: the binding policy decision for every evaluated tool call.
--
-- NULL preserves calls recorded before policy existed. New decisions are one
-- strict JSON value so verdict, reason, source, and the one-call scope ceiling
-- cannot be observed from different writes.
ALTER TABLE tool_calls ADD COLUMN policy_decision_json TEXT;
