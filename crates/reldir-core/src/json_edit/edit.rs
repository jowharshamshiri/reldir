//! Splicing changes into a document's bytes.
//!
//! Every edit owns a span of the original file and rewrites nothing outside
//! it. Edits are resolved against the original document all at once -- an
//! array index names an element as the file holds it, before any edit -- so
//! the order they are listed in decides nothing except, where several
//! insertions land at the same place, which comes first. Two edits that
//! would claim the same bytes, or one that depends on a member another
//! removes, are refused as a conflict rather than resolved by guesswork.
//!
//! Where each edit's bytes go:
//!
//! - A replacement takes the old value's span. Its nested lines are indented
//!   from the line the old value starts on. A non-empty container keeps its
//!   shape: written on one line, its replacement is too. A scalar or empty
//!   container is replaced by content written on one line when the container
//!   around it is on one line, and in the document's style otherwise.
//! - A removal takes the member or element with one adjacent comma and the
//!   whitespace that belonged to it: after the previous value, from that
//!   value's end; when first, up to where the next one begins. What remains is
//!   laid out as if the removed child had never been there; a container left
//!   with no children is written the way the style writes an empty one.
//! - An insertion goes after a sibling, on a line of its own indented like a
//!   sibling that has one, or with the container's own comma spacing when the
//!   container is on one line. Into an empty container -- or one whose every
//!   child is removed -- the whole container is written afresh.
//!
//! Every result is read back, and must hold exactly the value the edits
//! describe, member order included. A mismatch is a defect in this module and
//! is reported as [`EditError::Internal`] instead of being written.

use super::{
    EditError, finite, first_difference,
    render::{write_string, write_value},
    scan::{Children, Document, Kind, NodeId, Placed},
    style::Style,
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
};

/// One change to a document, addressed by RFC 6901 pointers into the
/// document as it stands before any edit.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Replace the value at `pointer`.
    Replace { pointer: String, value: Value },
    /// Remove the object member or array element at `pointer`.
    Remove { pointer: String },
    /// Add member `name` to the object at `pointer`, after the member named
    /// `after`, or after the last member when `after` is `None`.
    Insert {
        pointer: String,
        name: String,
        value: Value,
        after: Option<String>,
    },
    /// Add an element to the array at `pointer`, before the element now at
    /// `index`, or at the end when `index` is the array's length.
    ArrayInsert {
        pointer: String,
        index: usize,
        value: Value,
    },
}

impl Edit {
    pub fn pointer(&self) -> &str {
        match self {
            Edit::Replace { pointer, .. }
            | Edit::Remove { pointer }
            | Edit::Insert { pointer, .. }
            | Edit::ArrayInsert { pointer, .. } => pointer,
        }
    }

    fn value(&self) -> Option<&Value> {
        match self {
            Edit::Replace { value, .. }
            | Edit::Insert { value, .. }
            | Edit::ArrayInsert { value, .. } => Some(value),
            Edit::Remove { .. } => None,
        }
    }
}

/// Apply `edits` to the document in `raw`, changing no byte outside the spans
/// the edits own. With no edits, the bytes come back unchanged -- once they
/// are known to be a valid document.
pub fn apply(raw: &[u8], edits: &[Edit]) -> Result<Vec<u8>, EditError> {
    let document = Document::parse(raw)?;
    apply_to(&document, edits).map(|(bytes, _)| bytes)
}

/// [`apply`] on a parsed document, returning the new bytes and the value they
/// were verified to hold.
pub(super) fn apply_to(
    document: &Document<'_>,
    edits: &[Edit],
) -> Result<(Vec<u8>, Value), EditError> {
    if edits.is_empty() {
        return Ok((document.raw().to_vec(), document.value().clone()));
    }
    for value in edits.iter().filter_map(Edit::value) {
        check_finite(value)?;
    }
    let style = Style::detect(document.raw());
    let resolved = edits
        .iter()
        .map(|edit| resolve(document, edit))
        .collect::<Result<Vec<_>, _>>()?;
    let plan = Plan::of(document, edits, &resolved)?;
    let expected = document.assemble(&plan.replaced, &plan.arrangements(document))?;
    let splices = plan.splices(document, &style)?;
    let output = splice(document.raw(), splices)?;
    let reread = Document::parse(&output).map_err(|error| match error {
        EditError::Invalid(error) if crate::json::is_depth_limit(&error) => {
            EditError::TooDeep(error)
        }
        other => EditError::internal(format!(
            "the edited document does not read back ({other}): {:?}",
            String::from_utf8_lossy(&output)
        )),
    })?;
    if let Some(pointer) = first_difference(reread.value(), &expected) {
        return Err(EditError::internal(format!(
            "the edited document differs from the intended value at {pointer:?}: wrote {:?}, intended {}",
            String::from_utf8_lossy(&output),
            expected
        )));
    }
    Ok((output, expected))
}

fn check_finite(value: &Value) -> Result<(), EditError> {
    let mut work = vec![value];
    while let Some(value) = work.pop() {
        match value {
            Value::Number(number) => {
                let lexeme = number.to_string();
                if !finite(&lexeme) {
                    return Err(EditError::NonFinite(lexeme));
                }
            }
            Value::Array(elements) => work.extend(elements),
            Value::Object(members) => work.extend(members.values()),
            _ => {}
        }
    }
    Ok(())
}

/// An edit with its pointers resolved to nodes.
enum Resolved<'e> {
    Replace {
        node: NodeId,
        value: &'e Value,
    },
    Remove {
        node: NodeId,
        container: NodeId,
        index: usize,
    },
    Insert {
        container: NodeId,
        name: &'e str,
        value: &'e Value,
        /// The member, by position, the new one follows.
        after: Option<usize>,
        /// The position of an existing member of the same name.
        existing: Option<usize>,
    },
    ArrayInsert {
        container: NodeId,
        index: usize,
        value: &'e Value,
    },
}

impl Resolved<'_> {
    /// The node an edit's ownership is checked from: the container it
    /// changes, or for a replacement or removal, the container of the value
    /// it takes.
    fn container(&self, document: &Document<'_>) -> Option<NodeId> {
        match self {
            Resolved::Replace { node, .. } => document.node(*node).parent.map(|(parent, _)| parent),
            Resolved::Remove { container, .. }
            | Resolved::Insert { container, .. }
            | Resolved::ArrayInsert { container, .. } => Some(*container),
        }
    }

    /// The node whose whole span the edit takes, if it takes one.
    fn owned(&self) -> Option<NodeId> {
        match self {
            Resolved::Replace { node, .. } | Resolved::Remove { node, .. } => Some(*node),
            Resolved::Insert { .. } | Resolved::ArrayInsert { .. } => None,
        }
    }
}

fn resolve<'e>(document: &Document<'_>, edit: &'e Edit) -> Result<Resolved<'e>, EditError> {
    match edit {
        Edit::Replace { pointer, value } => Ok(Resolved::Replace {
            node: document.lookup(pointer)?,
            value,
        }),
        Edit::Remove { pointer } => {
            let node = document.lookup(pointer)?;
            let (container, index) = document.node(node).parent.ok_or(EditError::RootRemoval)?;
            Ok(Resolved::Remove {
                node,
                container,
                index,
            })
        }
        Edit::Insert {
            pointer,
            name,
            value,
            after,
        } => {
            let container = document.lookup(pointer)?;
            let Children::Members(members) = &document.node(container).children else {
                return Err(EditError::Kind {
                    pointer: pointer.clone(),
                    expected: "an object",
                    found: document.node(container).kind.describe(),
                });
            };
            let position = |name: &str| members.iter().position(|member| member.name == name);
            let after = match after {
                None => None,
                Some(sibling) => Some(position(sibling).ok_or_else(|| EditError::Missing {
                    pointer: format!("{pointer}/{}", crate::schema::path::escape_pointer(sibling)),
                })?),
            };
            Ok(Resolved::Insert {
                container,
                name,
                value,
                after,
                existing: position(name),
            })
        }
        Edit::ArrayInsert {
            pointer,
            index,
            value,
        } => {
            let container = document.lookup(pointer)?;
            let node = document.node(container);
            if node.kind != Kind::Array {
                return Err(EditError::Kind {
                    pointer: pointer.clone(),
                    expected: "an array",
                    found: node.kind.describe(),
                });
            }
            if *index > node.len() {
                return Err(EditError::Index {
                    pointer: pointer.clone(),
                    index: *index,
                    len: node.len(),
                });
            }
            Ok(Resolved::ArrayInsert {
                container,
                index: *index,
                value,
            })
        }
    }
}

/// A new child of a container: where it goes, and in what order among others
/// going to the same place.
struct Insertion<'e> {
    /// Before original child `slot`; the container's length for the end.
    slot: usize,
    order: usize,
    name: Option<&'e str>,
    value: &'e Value,
}

/// The children a container loses and gains.
#[derive(Default)]
struct Rearrangement<'e> {
    removed: BTreeSet<usize>,
    insertions: Vec<Insertion<'e>>,
}

struct Plan<'e> {
    replaced: HashMap<NodeId, &'e Value>,
    containers: Vec<(NodeId, Rearrangement<'e>)>,
}

impl<'e> Plan<'e> {
    fn of(
        document: &Document<'_>,
        edits: &[Edit],
        resolved: &[Resolved<'e>],
    ) -> Result<Self, EditError> {
        let describe = |index: usize| format!("{:?}", edits[index]);
        // Each replaced or removed node belongs to one edit, and nothing else
        // may happen inside it.
        let mut owners: HashMap<NodeId, usize> = HashMap::new();
        for (index, edit) in resolved.iter().enumerate() {
            if let Some(node) = edit.owned()
                && let Some(other) = owners.insert(node, index)
            {
                return Err(EditError::Conflict(format!(
                    "{} and {} both rewrite {:?}",
                    describe(other),
                    describe(index),
                    document.pointer(node)
                )));
            }
        }
        for (index, edit) in resolved.iter().enumerate() {
            let mut at = edit.container(document);
            while let Some(node) = at {
                if let Some(&owner) = owners.get(&node)
                    && owner != index
                {
                    return Err(EditError::Conflict(format!(
                        "{} changes what {} replaces or removes",
                        describe(index),
                        describe(owner)
                    )));
                }
                at = document.node(node).parent.map(|(parent, _)| parent);
            }
        }

        let mut replaced = HashMap::new();
        let mut containers: Vec<(NodeId, Rearrangement<'e>)> = vec![];
        let mut slot_of: HashMap<NodeId, usize> = HashMap::new();
        for edit in resolved {
            match edit {
                Resolved::Replace { node, value } => {
                    replaced.insert(*node, *value);
                }
                Resolved::Remove {
                    container, index, ..
                } => {
                    rearrangement(&mut containers, &mut slot_of, *container)
                        .removed
                        .insert(*index);
                }
                Resolved::Insert { .. } | Resolved::ArrayInsert { .. } => {}
            }
        }
        // Insertions are placed once every removal is known: "after the last
        // member" means after the last one that remains.
        let mut names: BTreeSet<(NodeId, &str)> = BTreeSet::new();
        for (order, edit) in resolved.iter().enumerate() {
            match edit {
                Resolved::Insert {
                    container,
                    name,
                    value,
                    after,
                    existing,
                } => {
                    let pointer = document.pointer(*container);
                    let changes = rearrangement(&mut containers, &mut slot_of, *container);
                    if let Some(existing) = existing
                        && !changes.removed.contains(existing)
                    {
                        return Err(EditError::Exists {
                            pointer,
                            name: (*name).to_string(),
                        });
                    }
                    if !names.insert((*container, name)) {
                        return Err(EditError::Conflict(format!(
                            "two edits insert member {name:?} into {pointer:?}"
                        )));
                    }
                    let slot = match after {
                        Some(after) if changes.removed.contains(after) => {
                            return Err(EditError::Conflict(format!(
                                "{} places a member after one that another edit removes",
                                describe(order)
                            )));
                        }
                        Some(after) => after + 1,
                        None => {
                            let len = document.node(*container).len();
                            (0..len)
                                .rev()
                                .find(|index| !changes.removed.contains(index))
                                .map_or(0, |last| last + 1)
                        }
                    };
                    changes.insertions.push(Insertion {
                        slot,
                        order,
                        name: Some(name),
                        value,
                    });
                }
                Resolved::ArrayInsert {
                    container,
                    index,
                    value,
                } => rearrangement(&mut containers, &mut slot_of, *container)
                    .insertions
                    .push(Insertion {
                        slot: *index,
                        order,
                        name: None,
                        value,
                    }),
                Resolved::Replace { .. } | Resolved::Remove { .. } => {}
            }
        }
        for (_, changes) in &mut containers {
            changes
                .insertions
                .sort_by_key(|insertion| (insertion.slot, insertion.order));
        }
        Ok(Self {
            replaced,
            containers,
        })
    }

    /// Each rearranged container's children, in their new order.
    fn arrangements(&self, document: &Document<'_>) -> HashMap<NodeId, Vec<Placed<'e>>> {
        let mut out = HashMap::new();
        for (container, changes) in &self.containers {
            let len = document.node(*container).len();
            let mut placed = vec![];
            let mut insertions = changes.insertions.iter().peekable();
            for index in 0..=len {
                while let Some(insertion) = insertions.next_if(|insertion| insertion.slot == index)
                {
                    placed.push(Placed::New {
                        name: insertion.name,
                        value: insertion.value,
                    });
                }
                if index < len && !changes.removed.contains(&index) {
                    placed.push(Placed::Kept(index));
                }
            }
            out.insert(*container, placed);
        }
        out
    }

    fn splices(&self, document: &Document<'_>, style: &Style) -> Result<Vec<Splice>, EditError> {
        let mut splices = vec![];
        for (node, value) in &self.replaced {
            splices.push(Splice {
                range: document.node(*node).span.clone(),
                text: render_in_place(document, style, *node, value),
            });
        }
        for (container, changes) in &self.containers {
            rearrange(document, style, *container, changes, &mut splices)?;
        }
        Ok(splices)
    }
}

/// The rearrangement planned for `container`, begun when first needed.
fn rearrangement<'p, 'e>(
    containers: &'p mut Vec<(NodeId, Rearrangement<'e>)>,
    slot_of: &mut HashMap<NodeId, usize>,
    container: NodeId,
) -> &'p mut Rearrangement<'e> {
    let slot = *slot_of.entry(container).or_insert_with(|| {
        containers.push((container, Rearrangement::default()));
        containers.len() - 1
    });
    &mut containers[slot].1
}

/// Bytes to put in place of a range of the original.
struct Splice {
    range: Range<usize>,
    text: Vec<u8>,
}

/// The original bytes with every splice made. Splices are made in position
/// order; insertions at one position keep the order they were planned in,
/// and come before a range that starts there.
fn splice(raw: &[u8], mut splices: Vec<Splice>) -> Result<Vec<u8>, EditError> {
    splices.sort_by_key(|splice| (splice.range.start, !splice.range.is_empty()));
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    for splice in splices {
        if splice.range.start < at {
            return Err(EditError::internal(format!(
                "planned splices overlap at byte {}",
                splice.range.start
            )));
        }
        out.extend_from_slice(&raw[at..splice.range.start]);
        out.extend_from_slice(&splice.text);
        at = splice.range.end;
    }
    out.extend_from_slice(&raw[at..]);
    Ok(out)
}

/// `value` written to stand where `node` stands now.
fn render_in_place(document: &Document<'_>, style: &Style, node: NodeId, value: &Value) -> Vec<u8> {
    let old = document.node(node);
    let single_line = style.is_compact()
        || if old.is_empty() {
            old.parent
                .is_some_and(|(parent, _)| document.single_line(document.node(parent).span.clone()))
        } else {
            document.single_line(old.span.clone())
        };
    let mut out = vec![];
    if single_line {
        write_value(&mut out, value, &style.single_line(), b"");
    } else {
        write_value(&mut out, value, style, document.line_indent(old.span.start));
    }
    out
}

/// How new children are separated from their siblings in one container.
enum Layout {
    /// Each on its own line, with this indentation.
    Lines(Vec<u8>),
    /// On the container's line, with this whitespace after each comma.
    Inline(Vec<u8>),
}

/// The layout of a new child placed beside child `reference`.
fn layout(document: &Document<'_>, style: &Style, container: NodeId, reference: usize) -> Layout {
    let node = document.node(container);
    let own_line = |index: usize| document.starts_own_line(container, index);
    let len = node.len();
    // The reference itself if it has its own line, else the nearest sibling
    // that does.
    let sibling = (0..len)
        .filter(|index| own_line(*index))
        .min_by_key(|index| index.abs_diff(reference));
    if let Some(sibling) = sibling {
        let start = node.child_start(sibling, document.nodes());
        return Layout::Lines(document.line_indent(start).to_vec());
    }
    // A one-line container: the whitespace after one of its own commas.
    let after_comma = [reference + 1, reference, 1]
        .into_iter()
        .find(|index| *index >= 1 && *index < len)
        .map(|index| {
            let entry = node.child_entry(index);
            document.raw()[entry.start..node.child_start(index, document.nodes())].to_vec()
        });
    Layout::Inline(after_comma.unwrap_or_else(|| style.comma.gap().as_bytes().to_vec()))
}

/// A new child's own text: its name and colon for a member, then its value.
fn entry_text(
    document: &Document<'_>,
    style: &Style,
    container: NodeId,
    reference: usize,
    layout: &Layout,
    insertion: &Insertion<'_>,
) -> Vec<u8> {
    let node = document.node(container);
    let mut out = vec![];
    if let (Some(name), Children::Members(members)) = (insertion.name, &node.children) {
        write_string(&mut out, name);
        // The reference member's own colon, spacing and all.
        let member = &members[reference];
        let value_start = document.node(member.value).span.start;
        out.extend_from_slice(&document.raw()[member.name_span.end..value_start]);
    }
    match layout {
        Layout::Lines(indentation) => write_value(&mut out, insertion.value, style, indentation),
        Layout::Inline(_) => write_value(&mut out, insertion.value, &style.single_line(), b""),
    }
    out
}

fn rearrange(
    document: &Document<'_>,
    style: &Style,
    container: NodeId,
    changes: &Rearrangement<'_>,
    splices: &mut Vec<Splice>,
) -> Result<(), EditError> {
    let node = document.node(container);
    let nodes = document.nodes();
    let len = node.len();
    let surviving: Vec<usize> = (0..len)
        .filter(|index| !changes.removed.contains(index))
        .collect();
    let value_end = |index: usize| nodes[node.child_value(index)].span.end;

    if surviving.is_empty() {
        if changes.insertions.is_empty() {
            // Every child removed: the container is written the way the
            // style writes an empty one.
            splices.push(Splice {
                range: node.span.start + 1..node.span.end - 1,
                text: style.empty.inner().as_bytes().to_vec(),
            });
        } else {
            // Nothing of the old content remains to sit beside: the
            // container is written afresh, holding only the new children.
            let value = if node.kind == Kind::Object {
                let mut members = Map::new();
                for insertion in &changes.insertions {
                    let name = insertion.name.ok_or_else(|| {
                        EditError::internal("an unnamed member was inserted into an object")
                    })?;
                    members.insert(name.to_string(), insertion.value.clone());
                }
                Value::Object(members)
            } else {
                Value::Array(
                    changes
                        .insertions
                        .iter()
                        .map(|insertion| insertion.value.clone())
                        .collect(),
                )
            };
            splices.push(Splice {
                range: node.span.clone(),
                text: render_in_place(document, style, container, &value),
            });
        }
        return Ok(());
    }

    // Each run of adjacent removed children goes with the comma before it,
    // or, at the start, with the comma after it.
    let removed: Vec<usize> = changes.removed.iter().copied().collect();
    let mut runs: Vec<(usize, usize)> = vec![];
    for index in removed {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == index => *last = index,
            _ => runs.push((index, index)),
        }
    }
    for (first, last) in runs {
        let range = if first > 0 {
            value_end(first - 1)..value_end(last)
        } else {
            node.child_start(0, nodes)..node.child_start(last + 1, nodes)
        };
        splices.push(Splice {
            range,
            text: vec![],
        });
    }

    for insertion in &changes.insertions {
        let anchor = surviving
            .iter()
            .rev()
            .find(|index| **index < insertion.slot)
            .copied();
        let reference = anchor.unwrap_or(surviving[0]);
        let layout = layout(document, style, container, reference);
        let entry = entry_text(document, style, container, reference, &layout, insertion);
        let separator = match &layout {
            Layout::Lines(indentation) => {
                let mut separator = style.newline.text().as_bytes().to_vec();
                separator.extend_from_slice(indentation);
                separator
            }
            Layout::Inline(gap) => gap.clone(),
        };
        let mut text = vec![];
        let position = match anchor {
            Some(anchor) => {
                text.push(b',');
                text.extend_from_slice(&separator);
                text.extend_from_slice(&entry);
                value_end(anchor)
            }
            None => {
                text.extend_from_slice(&entry);
                text.push(b',');
                text.extend_from_slice(&separator);
                node.child_start(0, nodes)
            }
        };
        splices.push(Splice {
            range: position..position,
            text,
        });
    }
    Ok(())
}
