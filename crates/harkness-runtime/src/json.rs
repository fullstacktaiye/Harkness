//! One order for the keys of an untyped JSON value.
//!
//! Harkness has several places where the *bytes* of a `serde_json::Value` are
//! the contract rather than the value: a tool result a hash is taken over, a
//! frozen scenario fixture, and the CLI's published envelope. All three were
//! byte-stable for free, because `serde_json::Map` is a `BTreeMap` and a
//! `BTreeMap` is sorted.
//!
//! That is not a property of `serde_json`. It is a property of one cargo
//! feature: enabling `preserve_order` swaps the map for an `IndexMap` that keeps
//! insertion order, and cargo unifies features across every member of a
//! workspace — so a crate two layers away adding a dependency changes the bytes
//! a different crate writes to disk. `agent-client-protocol-schema` requires
//! that feature, and ADR-0010 requires that crate, so this is not hypothetical:
//! the workspace has already lost the free version of the property once.
//!
//! Sorting explicitly is what buys it back, and buys it back for good. Keys are
//! compared by their exact UTF-8 bytes rather than by any locale or
//! character-wise ordering, so the order is a property of the value and not of
//! the platform that encoded it — the same rule
//! [`approval::canonical`](crate::approval) encodes an approval's input hash
//! under, and for the same reason.
//!
//! This is deliberately *not* the approval encoder. That one writes a canonical
//! byte string for hashing, refuses what it cannot encode, and is frozen by a
//! published domain constant; this one hands back a `Value` a caller goes on to
//! serialize however it likes.

use serde_json::Value;

/// Returns `value` with every object key, at every depth, in byte order.
///
/// Arrays keep their order, because an array's order is part of its meaning.
/// Scalars are returned unchanged.
///
/// ```
/// use serde_json::json;
///
/// let sorted = harkness_runtime::canonical_json(json!({
///     "zebra": 1,
///     "apple": {"yak": 2, "ant": 3},
/// }));
/// assert_eq!(
///     serde_json::to_string(&sorted).unwrap(),
///     r#"{"apple":{"ant":3,"yak":2},"zebra":1}"#,
/// );
/// ```
#[must_use]
pub fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut entries = fields.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, nested)| (key, canonical_json(nested)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json).collect()),
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::canonical_json;

    /// Nesting and arrays both, because an implementation that only reordered
    /// the outermost object would satisfy a flat assertion and still write two
    /// spellings of one value.
    #[test]
    fn every_object_is_sorted_at_every_depth() {
        let canonical = canonical_json(json!({
            "zebra": 1,
            "nested": {"yak": 2, "ant": 3},
            "listed": [{"yak": 4, "ant": 5}, 6],
            "apple": null,
        }));

        assert_eq!(
            serde_json::to_string(&canonical).unwrap(),
            r#"{"apple":null,"listed":[{"ant":5,"yak":4},6],"nested":{"ant":3,"yak":2},"zebra":1}"#,
        );
    }

    /// An array's order is part of its meaning and is never touched.
    #[test]
    fn arrays_keep_the_order_they_were_given() {
        let canonical = canonical_json(json!(["zebra", "apple", "mango"]));
        assert_eq!(canonical, json!(["zebra", "apple", "mango"]));
    }

    /// Sorting is by the key's exact bytes, so two spellings a locale-aware
    /// comparison folds together stay two keys in the order those bytes give.
    #[test]
    fn keys_sort_by_their_exact_bytes() {
        let canonical = canonical_json(json!({"b": 1, "B": 2, "_": 3, "ä": 4, "a": 5}));
        let keys = canonical
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(keys, ["B", "_", "a", "b", "ä"]);
    }

    /// Idempotent, which is what lets a caller canonicalize at whichever
    /// boundary it owns without having to know whether somebody already did.
    #[test]
    fn canonicalizing_twice_changes_nothing() {
        let once = canonical_json(json!({"z": {"b": 1, "a": 2}, "a": [{"d": 3, "c": 4}]}));
        assert_eq!(canonical_json(once.clone()), once);
    }
}
