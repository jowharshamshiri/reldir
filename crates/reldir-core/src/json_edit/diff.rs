//! The edits that turn one value into another.
//!
//! Comparison is by spelling: the crate reads numbers with arbitrary
//! precision, so `3` and `3.0`, `1.5` and `1.50`, `0` and `-0` are different
//! values, and strings compare by their characters, so an NFD string and its
//! NFC form differ. The one spelling serde_json does not keep is an
//! exponent's marker and sign -- `1E2`, `1e2` and `1e+2` all read as `1e+2` --
//! so those compare equal, and a file keeps whichever it wrote. Member order is
//! not compared -- a value whose members merely moved is not a change, and the
//! file keeps its order.
//!
//! Objects are compared member by member. Arrays are aligned element by
//! element with the fewest changes: an array that lost elements loses only
//! those, one that gained elements gains only those, and an element that
//! changed is edited in place. An array is never rewritten whole merely
//! because its change mixes those kinds -- that would rewrite, in the file's
//! style rather than as written, elements nobody changed.

use super::{
    EditError,
    edit::{Edit, apply_to},
    scan::Document,
};
use crate::schema::path::escape_pointer;
use serde_json::Value;

/// The edits that turn `before` -- the value a document holds, as
/// [`Document::value`] reads it -- into `after`, for [`super::apply`].
///
/// A new member is placed after the nearest member preceding it in `after`
/// that the document already has, so members added together stay together;
/// a new member that precedes every existing one goes at the end.
pub fn diff(before: &Value, after: &Value) -> Vec<Edit> {
    let mut edits = vec![];
    let mut work = vec![(String::new(), before, after)];
    while let Some((pointer, before, after)) = work.pop() {
        if before == after {
            continue;
        }
        match (before, after) {
            (Value::Object(old), Value::Object(new)) => {
                for name in old.keys() {
                    if !new.contains_key(name) {
                        edits.push(Edit::Remove {
                            pointer: format!("{pointer}/{}", escape_pointer(name)),
                        });
                    }
                }
                let mut anchor: Option<&String> = None;
                let mut at_end = vec![];
                for (name, value) in new {
                    match old.get(name) {
                        Some(previous) => {
                            anchor = Some(name);
                            if previous != value {
                                work.push((
                                    format!("{pointer}/{}", escape_pointer(name)),
                                    previous,
                                    value,
                                ));
                            }
                        }
                        None => {
                            let insert = Edit::Insert {
                                pointer: pointer.clone(),
                                name: name.clone(),
                                value: value.clone(),
                                after: anchor.cloned(),
                            };
                            if anchor.is_some() {
                                edits.push(insert);
                            } else {
                                at_end.push(insert);
                            }
                        }
                    }
                }
                // Listed after every anchored insertion, so they land after
                // anything placed behind the last member.
                edits.extend(at_end);
            }
            (Value::Array(old), Value::Array(new)) => {
                for step in align(old, new) {
                    match step {
                        Step::Change { old: at, new: to } => {
                            work.push((format!("{pointer}/{at}"), &old[at], &new[to]));
                        }
                        Step::Remove(at) => edits.push(Edit::Remove {
                            pointer: format!("{pointer}/{at}"),
                        }),
                        Step::Insert { before, new: from } => edits.push(Edit::ArrayInsert {
                            pointer: pointer.clone(),
                            index: before,
                            value: new[from].clone(),
                        }),
                    }
                }
            }
            _ => edits.push(Edit::Replace {
                pointer,
                value: after.clone(),
            }),
        }
    }
    edits
}

/// One step turning an old array into a new one. Equal elements are kept and
/// produce no step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Old element `old` becomes new element `new`, edited in place.
    Change { old: usize, new: usize },
    /// Old element `.0` goes.
    Remove(usize),
    /// New element `new` arrives before old element `before`.
    Insert { before: usize, new: usize },
}

/// The largest alignment table built, in cells: four megabytes. Arrays whose
/// differing middles are larger are aligned position by position, which is
/// still correct and still keeps every element that stays where it was.
const MAX_ALIGNMENT_CELLS: usize = 1 << 20;

/// What removing or inserting one element costs in an alignment.
const MOVE_COST: u32 = 2;

/// What changing `old` into `new` in place costs in an alignment: less the
/// more the two have in common, so an element is edited into the new element
/// it resembles rather than into an unrelated one. Containers of one kind
/// that differ in a single member or element cost no more than any scalar
/// change; containers differing more, or values of different kinds, cost
/// more -- but always less than removing one and inserting the other, which
/// is what a change in place saves.
fn change_cost(old: &Value, new: &Value) -> u32 {
    let differences = match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            let changed = old
                .iter()
                .filter(|(name, value)| new.get(*name) != Some(*value))
                .count();
            let added = new.keys().filter(|name| !old.contains_key(*name)).count();
            changed + added
        }
        (Value::Array(old), Value::Array(new)) => {
            let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            old.len() + new.len() - 2 * (prefix + suffix)
        }
        (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => {
            return MOVE_COST + 1;
        }
        _ => return MOVE_COST,
    };
    1 + differences.clamp(1, 2) as u32
}

/// The cheapest steps that turn `old` into `new`: an edit distance in which
/// keeping an equal element is free, removing or inserting one costs
/// [`MOVE_COST`] and changing one in place costs [`change_cost`]. A removal
/// leaves the elements around it as they were written, an insertion adds only
/// its own element, and a change rewrites only the element changed -- and,
/// through [`diff`], only the part of it that changed.
fn align(old: &[Value], new: &[Value]) -> Vec<Step> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];
    let (rows, columns) = (old_middle.len(), new_middle.len());
    let mut steps = vec![];
    if (rows + 1).saturating_mul(columns + 1) > MAX_ALIGNMENT_CELLS {
        for offset in 0..rows.max(columns) {
            steps.push(match (offset < rows, offset < columns) {
                (true, true) => Step::Change {
                    old: prefix + offset,
                    new: prefix + offset,
                },
                (true, false) => Step::Remove(prefix + offset),
                _ => Step::Insert {
                    before: prefix + rows,
                    new: prefix + offset,
                },
            });
        }
        return steps;
    }
    // distance[i][j]: the cost from old_middle[i..] to new_middle[j..].
    let width = columns + 1;
    let mut distance = vec![0u32; (rows + 1) * width];
    let cell = |i: usize, j: usize| i * width + j;
    let changing = |i: usize, j: usize| change_cost(&old_middle[i], &new_middle[j]);
    for i in (0..=rows).rev() {
        for j in (0..=columns).rev() {
            distance[cell(i, j)] = if i == rows {
                (columns - j) as u32 * MOVE_COST
            } else if j == columns {
                (rows - i) as u32 * MOVE_COST
            } else if old_middle[i] == new_middle[j] {
                distance[cell(i + 1, j + 1)]
            } else {
                (changing(i, j) + distance[cell(i + 1, j + 1)])
                    .min(MOVE_COST + distance[cell(i + 1, j)])
                    .min(MOVE_COST + distance[cell(i, j + 1)])
            };
        }
    }
    // Among equally cheap alignments, a removal or an insertion is preferred
    // to a change: it leaves the elements around it matched to themselves,
    // where a change would rewrite one element into another.
    let (mut i, mut j) = (0, 0);
    while i < rows || j < columns {
        let here = distance[cell(i, j)];
        if i < rows && j < columns && old_middle[i] == new_middle[j] {
            i += 1;
            j += 1;
        } else if i < rows && here == MOVE_COST + distance[cell(i + 1, j)] {
            steps.push(Step::Remove(prefix + i));
            i += 1;
        } else if j < columns && here == MOVE_COST + distance[cell(i, j + 1)] {
            steps.push(Step::Insert {
                before: prefix + i,
                new: prefix + j,
            });
            j += 1;
        } else {
            steps.push(Step::Change {
                old: prefix + i,
                new: prefix + j,
            });
            i += 1;
            j += 1;
        }
    }
    steps
}

/// The file `raw` changed to hold `after`, touching only what differs. When
/// nothing differs the bytes come back exactly as they were.
pub fn edit_document(raw: &[u8], after: &Value) -> Result<Vec<u8>, EditError> {
    let document = Document::parse(raw)?;
    if document.value() == after {
        return Ok(raw.to_vec());
    }
    let edits = diff(document.value(), after);
    let (bytes, written) = apply_to(&document, &edits)?;
    if &written != after {
        return Err(EditError::internal(format!(
            "the edits computed for {after} produced {written}"
        )));
    }
    Ok(bytes)
}
