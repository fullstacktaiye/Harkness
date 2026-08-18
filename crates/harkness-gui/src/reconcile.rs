//! Turning one keyed list into another as the smallest set of row edits.
//!
//! Two flat list models need the same thing: a projection arrives whole, and
//! replacing the rows with it destroys and rebuilds every delegate — throwing
//! away the scroll position, the hover state, and any dialog standing above a
//! row. `ChangesModel` reconciles the working tree that way and `ApprovalModel`
//! reconciles the pending approval queue, so the walk that decides *which* rows
//! actually moved lives here rather than once in each of them.
//!
//! Rows are keyed. A key is stable for as long as a row keeps its place in the
//! projection, which is what lets a refresh that clears three of five rows
//! remove exactly three and leave the other two alone. Only an order change or
//! a duplicate key falls back to a reset, because those are the two cases where
//! no set of insertions and removals honestly describes what happened.
//!
//! This module is pure: it plans, and the caller applies each step inside the
//! `beginInsertRows`/`endInsertRows` pair Qt requires for it. Keeping the plan
//! separate from the notification is what makes every rule here testable
//! without a `QGuiApplication`.

use std::collections::HashSet;

/// A row that carries its own identity.
pub(crate) trait Keyed {
    /// The token that is stable for as long as this row keeps its place.
    ///
    /// An empty key is legal and means "no identity", which
    /// [`plan`] treats as ambiguous the moment two rows share it.
    fn key(&self) -> &str;
}

/// One step of a reconciliation, in the row coordinates that hold when the step
/// is applied. Steps are recorded in the order they must be applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Edit<T> {
    /// Drop `first..=last`, inclusive.
    Remove { first: usize, last: usize },
    /// Insert `rows` starting at `first`.
    Insert { first: usize, rows: Vec<T> },
    /// Replace the `rows.len()` rows starting at `first`.
    Update { first: usize, rows: Vec<T> },
}

fn has_duplicate_keys<T: Keyed>(rows: &[T]) -> bool {
    let mut seen = HashSet::with_capacity(rows.len());
    !rows.iter().all(|row| seen.insert(row.key()))
}

/// Extends the run that ends at `position`, or starts a new one there.
fn extend_run<T>(
    run: &mut Option<(usize, Vec<T>)>,
    edits: &mut Vec<Edit<T>>,
    wrap: fn(usize, Vec<T>) -> Edit<T>,
    position: usize,
    row: T,
) {
    match run {
        Some((first, rows)) if *first + rows.len() == position => rows.push(row),
        _ => {
            flush_run(run, edits, wrap);
            *run = Some((position, vec![row]));
        }
    }
}

fn flush_run<T>(
    run: &mut Option<(usize, Vec<T>)>,
    edits: &mut Vec<Edit<T>>,
    wrap: fn(usize, Vec<T>) -> Edit<T>,
) {
    if let Some((first, rows)) = run.take() {
        edits.push(wrap(first, rows));
    }
}

fn insert_edit<T>(first: usize, rows: Vec<T>) -> Edit<T> {
    Edit::Insert { first, rows }
}

fn update_edit<T>(first: usize, rows: Vec<T>) -> Edit<T> {
    Edit::Update { first, rows }
}

/// Plans the edits that turn `current` into `incoming`, or `None` when the rows
/// have been reordered or carry ambiguous keys and only a reset is honest about
/// what happened.
pub(crate) fn plan<T: Keyed + Clone + PartialEq>(
    current: &[T],
    incoming: &[T],
) -> Option<Vec<Edit<T>>> {
    if has_duplicate_keys(current) || has_duplicate_keys(incoming) {
        return None;
    }
    let wanted = incoming.iter().map(Keyed::key).collect::<HashSet<_>>();
    let mut edits = Vec::new();

    // Removals are recorded back to front, so the index each one names is still
    // the index it has when the edits are applied in order.
    let mut retained = current.iter().collect::<Vec<_>>();
    let mut index = retained.len();
    while index > 0 {
        index -= 1;
        if wanted.contains(retained[index].key()) {
            continue;
        }
        let last = index;
        while index > 0 && !wanted.contains(retained[index - 1].key()) {
            index -= 1;
        }
        edits.push(Edit::Remove { first: index, last });
        retained.drain(index..=last);
    }

    // Every surviving row now has to appear in the incoming order. Walking both
    // in step identifies each incoming row as either the next survivor —
    // unchanged or updated in place — or an insertion.
    let mut cursor = 0;
    let mut insertion = None;
    let mut update = None;
    for (position, row) in incoming.iter().enumerate() {
        if cursor < retained.len() && retained[cursor].key() == row.key() {
            flush_run(&mut insertion, &mut edits, insert_edit);
            if retained[cursor] == row {
                flush_run(&mut update, &mut edits, update_edit);
            } else {
                extend_run(&mut update, &mut edits, update_edit, position, row.clone());
            }
            cursor += 1;
        } else {
            flush_run(&mut update, &mut edits, update_edit);
            extend_run(
                &mut insertion,
                &mut edits,
                insert_edit,
                position,
                row.clone(),
            );
        }
    }
    flush_run(&mut insertion, &mut edits, insert_edit);
    flush_run(&mut update, &mut edits, update_edit);

    // A survivor the incoming order never reached means the rows moved.
    (cursor == retained.len()).then_some(edits)
}

#[cfg(test)]
mod tests {
    use super::{Edit, Keyed, plan};

    /// The smallest thing this walk can reconcile: a key and a value.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Row {
        key: &'static str,
        value: &'static str,
    }

    impl Keyed for Row {
        fn key(&self) -> &str {
            self.key
        }
    }

    const fn row(key: &'static str, value: &'static str) -> Row {
        Row { key, value }
    }

    #[test]
    fn an_unchanged_projection_plans_no_edits() {
        let rows = [row("a", "one"), row("b", "two")];

        assert_eq!(plan(&rows, &rows.clone()), Some(Vec::new()));
    }

    #[test]
    fn a_row_that_left_the_projection_is_removed_alone() {
        let current = [row("a", "one"), row("b", "two"), row("c", "three")];
        let incoming = [current[0].clone(), current[2].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Remove { first: 1, last: 1 }])
        );
    }

    #[test]
    fn adjacent_removals_are_reported_as_one_run() {
        let current = [row("a", "one"), row("b", "two"), row("c", "three")];

        assert_eq!(
            plan(&current, &current[2..]),
            Some(vec![Edit::Remove { first: 0, last: 1 }])
        );
    }

    #[test]
    fn a_new_row_is_inserted_at_its_place_in_the_projection() {
        let current = [row("a", "one"), row("c", "three")];
        let added = row("b", "two");
        let incoming = [current[0].clone(), added.clone(), current[1].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Insert {
                first: 1,
                rows: vec![added],
            }])
        );
    }

    #[test]
    fn adjacent_insertions_are_reported_as_one_run() {
        let current = [row("a", "one")];
        let incoming = [current[0].clone(), row("b", "two"), row("c", "three")];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Insert {
                first: 1,
                rows: vec![row("b", "two"), row("c", "three")],
            }])
        );
    }

    #[test]
    fn a_row_whose_value_moved_updates_in_place() {
        let current = [row("a", "one"), row("b", "two")];
        let changed = row("b", "different");
        let incoming = [current[0].clone(), changed.clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Update {
                first: 1,
                rows: vec![changed],
            }])
        );
    }

    #[test]
    fn removals_insertions_and_updates_compose_in_application_order() {
        let current = [row("a", "one"), row("b", "two"), row("c", "three")];
        let changed = row("c", "changed");
        let added = row("d", "four");
        let incoming = [current[0].clone(), changed.clone(), added.clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![
                Edit::Remove { first: 1, last: 1 },
                Edit::Update {
                    first: 1,
                    rows: vec![changed],
                },
                Edit::Insert {
                    first: 2,
                    rows: vec![added],
                },
            ])
        );
    }

    #[test]
    fn emptying_the_projection_removes_every_row_in_one_run() {
        let current = [row("a", "one"), row("b", "two")];

        assert_eq!(
            plan(&current, &[]),
            Some(vec![Edit::Remove { first: 0, last: 1 }])
        );
    }

    #[test]
    fn a_reordered_projection_falls_back_to_a_reset() {
        let current = [row("a", "one"), row("b", "two")];
        let incoming = [current[1].clone(), current[0].clone()];

        assert_eq!(plan(&current, &incoming), None);
    }

    #[test]
    fn ambiguous_keys_fall_back_to_a_reset() {
        let duplicated = row("", "unkeyed");

        assert_eq!(plan(&[], &[duplicated.clone(), duplicated]), None);
    }
}
