//! Building, changing and comparing state trees.
//!
//! [`TreeBuilder`] builds a whole tree from the rows of a state.
//! [`TreeUpdate`] changes an existing tree by a delta, reading only the
//! leaves, mids and tables the delta touches and producing only the nodes
//! that differ -- everything else is shared with the tree it started from.
//! Both produce the same bytes for the same state, so a tree's ids never
//! depend on how it was reached.
//!
//! [`diff`] descends only into children whose object ids differ, so comparing
//! two states costs the buckets that changed between them; [`lookup`] reads
//! one path in four node reads; [`entries`] lists everything.
//!
//! Every node read here is proven against what its parent records -- a
//! table's, mid's or leaf's identity, and each row's placement -- so a tree
//! that is internally inconsistent is reported as corrupt, never built upon.

use super::{
    Hash256, Id, Object, ObjectKind, Store,
    error::{Invalid, Result, StoreError},
    node::{
        self, Child, Children, Entry, Extras, FANOUT, Leaf, Mid, Root, TableNode, TableRef, bucket,
        table_of, valid_path, valid_table_name,
    },
};
use std::collections::{BTreeMap, HashSet, btree_map};

/// A tree's root, its identity, and the nodes that are new in it.
#[derive(Debug, Clone)]
pub struct Built {
    pub root: Id,
    pub identity: Hash256,
    /// Each new node once, ready for a pack.
    pub objects: Vec<Object>,
}

/// Collects new nodes, each once.
#[derive(Default)]
struct Emit {
    seen: HashSet<Id>,
    objects: Vec<Object>,
}

impl Emit {
    /// Record a node's bytes unless `previous` -- the id the same position
    /// held before -- shows they are already stored.
    fn node(&mut self, bytes: Vec<u8>, previous: Option<Id>) -> Id {
        let id = Id::of(&bytes);
        if previous != Some(id) && self.seen.insert(id) {
            self.objects.push(Object::new(ObjectKind::Node, bytes));
        }
        id
    }
}

fn invalid(error: Invalid) -> StoreError {
    StoreError::Invalid(error)
}

fn check_row_path(path: &str, table: &str) -> Result<()> {
    if !valid_path(path) {
        return Err(invalid(Invalid::Path(path.into())));
    }
    if table_of(path) != Some(table) {
        return Err(invalid(Invalid::OutsideTable {
            path: path.into(),
            table: table.into(),
        }));
    }
    Ok(())
}

/// The child for a leaf holding `entries` (sorted), or `None` when empty.
fn leaf_child(entries: Vec<Entry>, previous: Option<Id>, emit: &mut Emit) -> Option<Child> {
    if entries.is_empty() {
        return None;
    }
    let leaf = Leaf { entries };
    let identity = leaf.identity();
    let id = emit.node(leaf.encode(), previous);
    Some(Child { id, identity })
}

/// The child for a mid with these children, or `None` when all are empty.
fn mid_child(children: Children, previous: Option<Id>, emit: &mut Emit) -> Option<Child> {
    if children.iter().all(Option::is_none) {
        return None;
    }
    let mid = Mid { children };
    let identity = mid.identity();
    let id = emit.node(mid.encode(), previous);
    Some(Child { id, identity })
}

fn table_ref(name: &str, node: &TableNode, previous: Option<Id>, emit: &mut Emit) -> TableRef {
    TableRef {
        name: name.to_string(),
        identity: node.identity(),
        id: emit.node(node.encode(), previous),
    }
}

fn extras_id(entries: BTreeMap<String, Id>, previous: Option<Id>, emit: &mut Emit) -> Option<Id> {
    if entries.is_empty() {
        return None;
    }
    let node = Extras {
        entries: entries.into_iter().collect(),
    };
    Some(emit.node(node.encode(), previous))
}

fn finish_root(root: Root, previous: Option<Id>, mut emit: Emit) -> Built {
    let identity = root.identity();
    let id = emit.node(root.encode(), previous);
    Built {
        root: id,
        identity,
        objects: emit.objects,
    }
}

#[derive(Debug, Default)]
struct TableRows {
    schema: Option<Hash256>,
    rows: BTreeMap<String, (Hash256, Id)>,
}

/// Builds a whole tree from a state's parts.
#[derive(Debug, Default)]
pub struct TreeBuilder {
    format: Option<Hash256>,
    config: Option<Hash256>,
    tables: BTreeMap<String, TableRows>,
    extras: BTreeMap<String, Id>,
}

impl TreeBuilder {
    pub fn new(format: Option<Hash256>, config: Option<Hash256>) -> Self {
        Self {
            format,
            config,
            ..Self::default()
        }
    }

    /// Declare a table. A table exists in the state whether or not it has
    /// rows, so every table is declared, and declared once.
    pub fn table(&mut self, name: &str, schema: Option<Hash256>) -> Result<()> {
        if !valid_table_name(name) {
            return Err(invalid(Invalid::TableName(name.into())));
        }
        match self.tables.entry(name.to_string()) {
            btree_map::Entry::Occupied(_) => Err(invalid(Invalid::Duplicate(name.into()))),
            btree_map::Entry::Vacant(slot) => {
                slot.insert(TableRows {
                    schema,
                    rows: BTreeMap::new(),
                });
                Ok(())
            }
        }
    }

    /// Add a row of a declared table.
    pub fn insert(&mut self, entry: Entry) -> Result<()> {
        let table = table_of(&entry.path)
            .filter(|_| valid_path(&entry.path))
            .ok_or_else(|| invalid(Invalid::Path(entry.path.clone())))?;
        let rows = &mut self
            .tables
            .get_mut(table)
            .ok_or_else(|| invalid(Invalid::UnknownTable(table.into())))?
            .rows;
        match rows.entry(entry.path) {
            btree_map::Entry::Occupied(slot) => {
                Err(invalid(Invalid::Duplicate(slot.key().clone())))
            }
            btree_map::Entry::Vacant(slot) => {
                slot.insert((entry.row_hash, entry.blob));
                Ok(())
            }
        }
    }

    pub fn extend(&mut self, entries: impl IntoIterator<Item = Entry>) -> Result<()> {
        entries.into_iter().try_for_each(|entry| self.insert(entry))
    }

    /// Keep a file that is not a valid row with the state.
    pub fn extra(&mut self, path: &str, blob: Id) -> Result<()> {
        if !valid_path(path) {
            return Err(invalid(Invalid::Path(path.into())));
        }
        match self.extras.insert(path.to_string(), blob) {
            Some(_) => Err(invalid(Invalid::Duplicate(path.into()))),
            None => Ok(()),
        }
    }

    pub fn finish(self) -> Result<Built> {
        for path in self.extras.keys() {
            let is_row = table_of(path)
                .and_then(|table| self.tables.get(table))
                .is_some_and(|table| table.rows.contains_key(path));
            if is_row {
                return Err(invalid(Invalid::RowAndExtra(path.clone())));
            }
        }
        let mut emit = Emit::default();
        let mut tables = vec![];
        for (name, table) in self.tables {
            let mut buckets: BTreeMap<(usize, usize), Vec<Entry>> = BTreeMap::new();
            for (path, (row_hash, blob)) in table.rows {
                buckets.entry(bucket(&path)).or_default().push(Entry {
                    path,
                    row_hash,
                    blob,
                });
            }
            // On the heap: 64 sets of 64 children is too large for a stack.
            let mut mids: Vec<Children> = vec![[None; FANOUT]; FANOUT];
            for ((mid, leaf), entries) in buckets {
                mids[mid][leaf] = leaf_child(entries, None, &mut emit);
            }
            let mut node = TableNode {
                schema: table.schema,
                mids: [None; FANOUT],
            };
            for (slot, children) in node.mids.iter_mut().zip(mids) {
                *slot = mid_child(children, None, &mut emit);
            }
            tables.push(table_ref(&name, &node, None, &mut emit));
        }
        let extras = extras_id(self.extras, None, &mut emit);
        let root = Root {
            format: self.format,
            config: self.config,
            extras,
            tables,
        };
        Ok(finish_root(root, None, emit))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOp {
    Add(Hash256, Id),
    Modify(Hash256, Id),
    Remove,
}

#[derive(Debug, Default)]
struct TableDelta {
    /// `Some(schema)` when the delta creates the table.
    create: Option<Option<Hash256>>,
    schema: Option<Option<Hash256>>,
    drop: bool,
    rows: BTreeMap<String, RowOp>,
}

/// A change to an existing tree.
pub struct TreeUpdate<'s, 'a> {
    store: &'s Store<'a>,
    base: Id,
    root: Root,
    format: Option<Hash256>,
    config: Option<Hash256>,
    tables: BTreeMap<String, TableDelta>,
    extras: BTreeMap<String, Option<Id>>,
}

impl<'s, 'a> TreeUpdate<'s, 'a> {
    /// Begin changing the tree rooted at `root`.
    pub fn new(store: &'s Store<'a>, root: Id) -> Result<Self> {
        let node = node::load_root(store, &root)?;
        Ok(Self {
            store,
            base: root,
            format: node.format,
            config: node.config,
            root: node,
            tables: BTreeMap::new(),
            extras: BTreeMap::new(),
        })
    }

    pub fn set_format(&mut self, format: Option<Hash256>) {
        self.format = format;
    }

    pub fn set_config(&mut self, config: Option<Hash256>) {
        self.config = config;
    }

    fn in_base(&self, name: &str) -> bool {
        self.root.table(name).is_some()
    }

    /// The delta for an existing (or created), undropped table. Checked
    /// before anything is recorded: a refused call leaves the update as it
    /// was, so no table appears because a row was refused for it.
    fn live_delta(&mut self, name: &str) -> Result<&mut TableDelta> {
        let delta = self.tables.get(name);
        if delta.is_some_and(|d| d.drop) {
            return Err(invalid(Invalid::ConflictingDelta(name.into())));
        }
        if !self.in_base(name) && !delta.is_some_and(|d| d.create.is_some()) {
            return Err(invalid(Invalid::UnknownTable(name.into())));
        }
        Ok(self.tables.entry(name.to_string()).or_default())
    }

    pub fn create_table(&mut self, name: &str, schema: Option<Hash256>) -> Result<()> {
        if !valid_table_name(name) {
            return Err(invalid(Invalid::TableName(name.into())));
        }
        if self.in_base(name) && !self.tables.get(name).is_some_and(|d| d.drop) {
            return Err(invalid(Invalid::TableExists(name.into())));
        }
        let delta = self.tables.entry(name.to_string()).or_default();
        if delta.create.is_some() || delta.drop {
            return Err(invalid(Invalid::ConflictingDelta(name.into())));
        }
        delta.create = Some(schema);
        Ok(())
    }

    pub fn set_schema(&mut self, name: &str, schema: Option<Hash256>) -> Result<()> {
        let delta = self.live_delta(name)?;
        if delta.schema.is_some() || delta.create.is_some() {
            return Err(invalid(Invalid::ConflictingDelta(name.into())));
        }
        delta.schema = Some(schema);
        Ok(())
    }

    /// Remove a table and every row in it. Nothing else may change the table
    /// in the same delta.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        if !self.in_base(name) {
            return Err(invalid(Invalid::UnknownTable(name.into())));
        }
        let delta = self.tables.entry(name.to_string()).or_default();
        if delta.drop || delta.create.is_some() || delta.schema.is_some() || !delta.rows.is_empty()
        {
            return Err(invalid(Invalid::ConflictingDelta(name.into())));
        }
        delta.drop = true;
        Ok(())
    }

    fn row(&mut self, path: &str, op: RowOp) -> Result<()> {
        let table = table_of(path)
            .filter(|_| valid_path(path))
            .ok_or_else(|| invalid(Invalid::Path(path.into())))?;
        check_row_path(path, table)?;
        let delta = self.live_delta(table)?;
        match delta.rows.entry(path.to_string()) {
            btree_map::Entry::Occupied(_) => Err(invalid(Invalid::ConflictingDelta(path.into()))),
            btree_map::Entry::Vacant(slot) => {
                slot.insert(op);
                Ok(())
            }
        }
    }

    /// A row that must not be present before.
    pub fn add(&mut self, entry: Entry) -> Result<()> {
        self.row(&entry.path, RowOp::Add(entry.row_hash, entry.blob))
    }

    /// A row that must be present before.
    pub fn modify(&mut self, entry: Entry) -> Result<()> {
        self.row(&entry.path, RowOp::Modify(entry.row_hash, entry.blob))
    }

    /// A row that must be present before.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.row(path, RowOp::Remove)
    }

    /// Keep (or replace) a file that is not a valid row.
    pub fn set_extra(&mut self, path: &str, blob: Id) -> Result<()> {
        self.extra(path, Some(blob))
    }

    /// Forget an extra file, which must be present before.
    pub fn remove_extra(&mut self, path: &str) -> Result<()> {
        self.extra(path, None)
    }

    fn extra(&mut self, path: &str, blob: Option<Id>) -> Result<()> {
        if !valid_path(path) {
            return Err(invalid(Invalid::Path(path.into())));
        }
        match self.extras.entry(path.to_string()) {
            btree_map::Entry::Occupied(_) => Err(invalid(Invalid::ConflictingDelta(path.into()))),
            btree_map::Entry::Vacant(slot) => {
                slot.insert(blob);
                Ok(())
            }
        }
    }

    pub fn finish(self) -> Result<Built> {
        let store = self.store;
        let base_extras = match &self.root.extras {
            Some(id) if self.needs_base_extras() => {
                node::load_extras(store, id)?.entries.into_iter().collect()
            }
            _ => BTreeMap::new(),
        };
        self.check_rows_and_extras(&base_extras)?;

        let mut emit = Emit::default();
        let mut tables = vec![];
        let mut deltas = self.tables;
        for base in &self.root.tables {
            match deltas.remove(&base.name) {
                None => tables.push(base.clone()),
                Some(delta) if delta.drop => {}
                Some(delta) => tables.push(apply_table(
                    store,
                    Some(base),
                    &base.name,
                    delta,
                    &mut emit,
                )?),
            }
        }
        // What remains names no base table, so only a creation may.
        for (name, delta) in deltas {
            if delta.create.is_none() {
                return Err(invalid(Invalid::UnknownTable(name)));
            }
            tables.push(apply_table(store, None, &name, delta, &mut emit)?);
        }
        tables.sort_by(|a, b| a.name.cmp(&b.name));

        let extras = if self.extras.is_empty() {
            self.root.extras
        } else {
            // Loaded whole above whenever the delta touches extras.
            let mut entries = base_extras;
            for (path, change) in self.extras {
                match change {
                    Some(blob) => {
                        entries.insert(path, blob);
                    }
                    None => {
                        if entries.remove(&path).is_none() {
                            return Err(invalid(Invalid::NotPresent(path)));
                        }
                    }
                }
            }
            extras_id(entries, self.root.extras, &mut emit)
        };

        let root = Root {
            format: self.format,
            config: self.config,
            extras,
            tables,
        };
        Ok(finish_root(root, Some(self.base), emit))
    }

    /// Whether the base's extras must be read: to change them, or to prove
    /// that no row the delta writes is also an extra file.
    fn needs_base_extras(&self) -> bool {
        !self.extras.is_empty()
            || self
                .tables
                .values()
                .any(|d| d.rows.values().any(|op| *op != RowOp::Remove))
    }

    /// A path is a row or an extra file, never both, after the delta.
    fn check_rows_and_extras(&self, base_extras: &BTreeMap<String, Id>) -> Result<()> {
        for delta in self.tables.values() {
            for (path, op) in &delta.rows {
                if *op == RowOp::Remove {
                    continue;
                }
                let extra_after = match self.extras.get(path) {
                    Some(change) => change.is_some(),
                    None => base_extras.contains_key(path),
                };
                if extra_after {
                    return Err(invalid(Invalid::RowAndExtra(path.clone())));
                }
            }
        }
        for (path, change) in &self.extras {
            let Some(table) = table_of(path).filter(|_| change.is_some()) else {
                continue;
            };
            let delta = self.tables.get(table);
            let row_after = match delta.and_then(|d| d.rows.get(path)) {
                Some(RowOp::Remove) => false,
                Some(_) => true,
                // Untouched by the delta: a row after exactly when it was one
                // before, in a table neither dropped nor newly created.
                None if delta.is_some_and(|d| d.drop || d.create.is_some()) => false,
                None => match self.root.table(table) {
                    Some(base) => lookup_in_table(self.store, base, path)?.is_some(),
                    None => false,
                },
            };
            if row_after {
                return Err(invalid(Invalid::RowAndExtra(path.clone())));
            }
        }
        Ok(())
    }
}

/// Apply one table's delta. `base` is the table as the root recorded it, or
/// `None` for a table the delta creates.
fn apply_table(
    store: &Store<'_>,
    base: Option<&TableRef>,
    name: &str,
    delta: TableDelta,
    emit: &mut Emit,
) -> Result<TableRef> {
    let mut table = match base {
        Some(base) => node::load_table(store, base)?,
        None => TableNode {
            schema: delta.create.flatten(),
            mids: [None; FANOUT],
        },
    };
    if let Some(schema) = delta.schema {
        table.schema = schema;
    }
    let mut grouped: BTreeMap<usize, BTreeMap<usize, Vec<(String, RowOp)>>> = BTreeMap::new();
    for (path, op) in delta.rows {
        let (mid, leaf) = bucket(&path);
        grouped
            .entry(mid)
            .or_default()
            .entry(leaf)
            .or_default()
            .push((path, op));
    }
    for (mid_index, leaves) in grouped {
        let previous_mid = table.mids[mid_index];
        let mut children = match &previous_mid {
            Some(child) => node::load_mid(store, child)?.children,
            None => [None; FANOUT],
        };
        for (leaf_index, ops) in leaves {
            let previous_leaf = children[leaf_index];
            let mut rows: BTreeMap<String, (Hash256, Id)> = match &previous_leaf {
                Some(child) => node::load_leaf(store, child, name, mid_index, leaf_index)?
                    .entries
                    .into_iter()
                    .map(|e| (e.path, (e.row_hash, e.blob)))
                    .collect(),
                None => BTreeMap::new(),
            };
            for (path, op) in ops {
                let present = rows.contains_key(&path);
                match op {
                    RowOp::Add(..) if present => {
                        return Err(invalid(Invalid::AlreadyPresent(path)));
                    }
                    RowOp::Modify(..) | RowOp::Remove if !present => {
                        return Err(invalid(Invalid::NotPresent(path)));
                    }
                    RowOp::Add(row_hash, blob) | RowOp::Modify(row_hash, blob) => {
                        rows.insert(path, (row_hash, blob));
                    }
                    RowOp::Remove => {
                        rows.remove(&path);
                    }
                }
            }
            let entries = rows
                .into_iter()
                .map(|(path, (row_hash, blob))| Entry {
                    path,
                    row_hash,
                    blob,
                })
                .collect();
            children[leaf_index] = leaf_child(entries, previous_leaf.map(|c| c.id), emit);
        }
        table.mids[mid_index] = mid_child(children, previous_mid.map(|c| c.id), emit);
    }
    Ok(table_ref(name, &table, base.map(|b| b.id), emit))
}

/// The row at `path` in a table, reading three nodes.
fn lookup_in_table(
    store: &Store<'_>,
    table: &TableRef,
    path: &str,
) -> Result<Option<(Hash256, Id)>> {
    let node = node::load_table(store, table)?;
    let (mid, leaf) = bucket(path);
    let Some(mid_child) = node.mids[mid] else {
        return Ok(None);
    };
    let Some(leaf_child) = node::load_mid(store, &mid_child)?.children[leaf] else {
        return Ok(None);
    };
    let leaf = node::load_leaf(store, &leaf_child, &table.name, mid, leaf)?;
    Ok(leaf.find(path).map(|e| (e.row_hash, e.blob)))
}

/// The row at `path` in the state rooted at `root`: its hash and blob.
pub fn lookup(store: &Store<'_>, root: &Id, path: &str) -> Result<Option<(Hash256, Id)>> {
    let table = table_of(path)
        .filter(|_| valid_path(path))
        .ok_or_else(|| invalid(Invalid::Path(path.into())))?;
    let root = node::load_root(store, root)?;
    match root.table(table) {
        Some(table) => lookup_in_table(store, table, path),
        None => Ok(None),
    }
}

fn table_entries(store: &Store<'_>, table: &TableRef, out: &mut Vec<Entry>) -> Result<()> {
    let node = node::load_table(store, table)?;
    for (mid_index, mid) in node.mids.iter().enumerate() {
        let Some(mid) = mid else { continue };
        let children = node::load_mid(store, mid)?.children;
        for (leaf_index, leaf) in children.iter().enumerate() {
            let Some(leaf) = leaf else { continue };
            out.extend(node::load_leaf(store, leaf, &table.name, mid_index, leaf_index)?.entries);
        }
    }
    Ok(())
}

/// Every row of the state rooted at `root`, in path order.
pub fn entries(store: &Store<'_>, root: &Id) -> Result<Vec<Entry>> {
    let root = node::load_root(store, root)?;
    let mut out = vec![];
    for table in &root.tables {
        table_entries(store, table, &mut out)?;
    }
    // Leaves are in bucket order, and tables in name order -- which is not
    // path order when a name is a prefix of another followed by a byte below
    // `/` (`a` and `a-b`).
    out.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The files kept with the state rooted at `root` that are not rows.
pub fn extras(store: &Store<'_>, root: &Id) -> Result<Vec<(String, Id)>> {
    match node::load_root(store, root)?.extras {
        Some(id) => Ok(node::load_extras(store, &id)?.entries),
        None => Ok(vec![]),
    }
}

/// One path whose row differs between two states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// The row hash and blob before; `None` when added.
    pub before: Option<(Hash256, Id)>,
    /// The row hash and blob after; `None` when removed.
    pub after: Option<(Hash256, Id)>,
}

/// Merge two leaves' entries, both in path order.
fn diff_leaves(before: Vec<Entry>, after: Vec<Entry>, out: &mut Vec<Change>) {
    use std::cmp::Ordering;
    let row = |e: &Entry| Some((e.row_hash, e.blob));
    let (mut b, mut a) = (0, 0);
    while b < before.len() || a < after.len() {
        let order = match (before.get(b), after.get(a)) {
            (Some(x), Some(y)) => x.path.cmp(&y.path),
            (Some(_), None) => Ordering::Less,
            _ => Ordering::Greater,
        };
        let change = match order {
            Ordering::Less => {
                b += 1;
                Change {
                    path: before[b - 1].path.clone(),
                    before: row(&before[b - 1]),
                    after: None,
                }
            }
            Ordering::Greater => {
                a += 1;
                Change {
                    path: after[a - 1].path.clone(),
                    before: None,
                    after: row(&after[a - 1]),
                }
            }
            Ordering::Equal => {
                b += 1;
                a += 1;
                Change {
                    path: before[b - 1].path.clone(),
                    before: row(&before[b - 1]),
                    after: row(&after[a - 1]),
                }
            }
        };
        if change.before != change.after {
            out.push(change);
        }
    }
}

fn leaf_entries(
    store: &Store<'_>,
    child: Option<Child>,
    table: &str,
    mid: usize,
    leaf: usize,
) -> Result<Vec<Entry>> {
    match child {
        Some(child) => Ok(node::load_leaf(store, &child, table, mid, leaf)?.entries),
        None => Ok(vec![]),
    }
}

fn mid_children(store: &Store<'_>, child: Option<Child>) -> Result<Children> {
    match child {
        Some(child) => Ok(node::load_mid(store, &child)?.children),
        None => Ok([None; FANOUT]),
    }
}

fn diff_tables(
    store: &Store<'_>,
    name: &str,
    before: Option<&TableRef>,
    after: Option<&TableRef>,
    out: &mut Vec<Change>,
) -> Result<()> {
    let load = |table: Option<&TableRef>| match table {
        Some(table) => node::load_table(store, table).map(|t| t.mids),
        None => Ok([None; FANOUT]),
    };
    let (before, after) = (load(before)?, load(after)?);
    for mid in 0..FANOUT {
        if before[mid].map(|c| c.id) == after[mid].map(|c| c.id) {
            continue;
        }
        let (b, a) = (
            mid_children(store, before[mid])?,
            mid_children(store, after[mid])?,
        );
        for leaf in 0..FANOUT {
            if b[leaf].map(|c| c.id) == a[leaf].map(|c| c.id) {
                continue;
            }
            diff_leaves(
                leaf_entries(store, b[leaf], name, mid, leaf)?,
                leaf_entries(store, a[leaf], name, mid, leaf)?,
                out,
            );
        }
    }
    Ok(())
}

/// The rows that differ between the states rooted at `before` and `after`,
/// in path order. A row whose blob alone changed -- the same logical row,
/// written differently -- is a change too: the caller decides whether
/// formatting matters to it.
pub fn diff(store: &Store<'_>, before: &Id, after: &Id) -> Result<Vec<Change>> {
    if before == after {
        return Ok(vec![]);
    }
    let (b, a) = (
        node::load_root(store, before)?,
        node::load_root(store, after)?,
    );
    let mut names: Vec<&str> = b
        .tables
        .iter()
        .chain(a.tables.iter())
        .map(|t| t.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut out = vec![];
    for name in names {
        let (tb, ta) = (b.table(name), a.table(name));
        if tb.map(|t| t.id) == ta.map(|t| t.id) {
            continue;
        }
        diff_tables(store, name, tb, ta, &mut out)?;
    }
    out.sort_unstable_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::fs::{Entry as DirEntry, Fs, Meta, Source};
    use crate::store::{
        Corruption,
        node::empty_table_identity,
        pack::{PackName, PackWriter, publish},
        test_fs::MemFs,
    };
    use proptest::prelude::*;
    use std::{
        collections::BTreeSet,
        io,
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    pub(crate) const OBJECTS: &str = "/db/.db/objects";
    const PACKS: &str = "/db/.db/objects/pack";
    const STAGING: &str = "/staging";

    pub(crate) fn mem() -> MemFs {
        MemFs::with_dirs(&[Path::new(PACKS), Path::new(STAGING)])
    }

    /// Publish objects as one pack.
    pub(crate) fn save(fs: &dyn Fs, objects: &[Object]) {
        if objects.is_empty() {
            return;
        }
        let mut writer = PackWriter::new();
        for object in objects {
            writer.add_object(object.clone()).unwrap();
        }
        let name = PackName::generate();
        writer.write_to(fs, Path::new(STAGING), &name).unwrap();
        publish(fs, Path::new(STAGING), Path::new(PACKS), &name).unwrap();
    }

    pub(crate) fn row(path: &str, row: u8, blob: u8) -> Entry {
        Entry {
            path: path.into(),
            row_hash: Hash256::of(&[row]),
            blob: Id::of(&[row, blob]),
        }
    }

    /// A whole state, as plain maps: what the tree must agree with.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct State {
        pub format: Option<Hash256>,
        pub config: Option<Hash256>,
        pub tables: BTreeMap<String, Option<Hash256>>,
        pub rows: BTreeMap<String, (Hash256, Id)>,
        pub extras: BTreeMap<String, Id>,
    }

    pub(crate) fn build(state: &State) -> Built {
        let mut builder = TreeBuilder::new(state.format, state.config);
        for (name, schema) in &state.tables {
            builder.table(name, *schema).unwrap();
        }
        for (path, (row_hash, blob)) in &state.rows {
            builder
                .insert(Entry {
                    path: path.clone(),
                    row_hash: *row_hash,
                    blob: *blob,
                })
                .unwrap();
        }
        for (path, blob) in &state.extras {
            builder.extra(path, *blob).unwrap();
        }
        builder.finish().unwrap()
    }

    /// Counts positioned reads -- one per object read -- of the source it wraps.
    pub(crate) struct Counting<'a> {
        pub inner: &'a MemFs,
        pub reads: AtomicUsize,
    }

    impl Source for Counting<'_> {
        fn metadata(&self, path: &Path) -> io::Result<Meta> {
            self.inner.metadata(path)
        }
        fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
            self.inner.read_dir(path)
        }
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.inner.read(path)
        }
        fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read_at(path, offset, len)
        }
    }

    impl Counting<'_> {
        pub fn take(&self) -> usize {
            self.reads.swap(0, Ordering::SeqCst)
        }
    }

    fn invalid_of<T: std::fmt::Debug>(result: Result<T>) -> Invalid {
        match result {
            Err(StoreError::Invalid(invalid)) => invalid,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test8250_the_builder_refuses_what_a_state_cannot_hold() {
        let mut builder = TreeBuilder::new(None, None);
        builder.table("t", None).unwrap();
        assert_eq!(
            invalid_of(builder.table("t", None)),
            Invalid::Duplicate("t".into())
        );
        assert_eq!(
            invalid_of(builder.table("a/b", None)),
            Invalid::TableName("a/b".into())
        );
        assert_eq!(
            invalid_of(builder.insert(row("u/a.json", 1, 1))),
            Invalid::UnknownTable("u".into())
        );
        assert_eq!(
            invalid_of(builder.insert(row("t/../x", 1, 1))),
            Invalid::Path("t/../x".into())
        );
        assert_eq!(
            invalid_of(builder.insert(row("t", 1, 1))),
            Invalid::Path("t".into())
        );
        builder.insert(row("t/a.json", 1, 1)).unwrap();
        assert_eq!(
            invalid_of(builder.insert(row("t/a.json", 2, 2))),
            Invalid::Duplicate("t/a.json".into())
        );
        assert_eq!(
            invalid_of(builder.extra("/abs", Id::of(b"x"))),
            Invalid::Path("/abs".into())
        );
        builder.extra("t/a.json", Id::of(b"x")).unwrap();
        assert_eq!(
            invalid_of(builder.finish()),
            Invalid::RowAndExtra("t/a.json".into())
        );
    }

    #[test]
    fn test8251_a_delta_is_held_to_the_tree_it_changes() {
        let fs = mem();
        let mut state = State::default();
        state.tables.insert("t".into(), None);
        state.tables.insert("u".into(), None);
        state
            .rows
            .insert("t/a.json".into(), (Hash256::of(b"a"), Id::of(b"a")));
        state.extras.insert("t/bad.json".into(), Id::of(b"bad"));
        let base = build(&state);
        save(&fs, &base.objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();
        let update = || TreeUpdate::new(&store, base.root).unwrap();

        let mut u = update();
        u.add(row("t/a.json", 1, 1)).unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::AlreadyPresent("t/a.json".into())
        );
        let mut u = update();
        u.modify(row("t/b.json", 1, 1)).unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::NotPresent("t/b.json".into())
        );
        let mut u = update();
        u.remove("t/b.json").unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::NotPresent("t/b.json".into())
        );

        let mut u = update();
        u.remove("t/a.json").unwrap();
        assert_eq!(
            invalid_of(u.modify(row("t/a.json", 1, 1))),
            Invalid::ConflictingDelta("t/a.json".into())
        );
        assert_eq!(
            invalid_of(u.add(row("v/a.json", 1, 1))),
            Invalid::UnknownTable("v".into())
        );
        assert_eq!(
            invalid_of(u.create_table("u", None)),
            Invalid::TableExists("u".into())
        );
        assert_eq!(
            invalid_of(u.drop_table("t")),
            Invalid::ConflictingDelta("t".into())
        );
        u.drop_table("u").unwrap();
        assert_eq!(
            invalid_of(u.add(row("u/a.json", 1, 1))),
            Invalid::ConflictingDelta("u".into())
        );
        assert_eq!(
            invalid_of(u.set_schema("u", None)),
            Invalid::ConflictingDelta("u".into())
        );
        assert_eq!(
            invalid_of(u.drop_table("zzz")),
            Invalid::UnknownTable("zzz".into())
        );

        // A refused change leaves nothing behind: no table appears because a
        // row or schema was refused for it.
        let mut u = update();
        assert_eq!(
            invalid_of(u.add(row("v/a.json", 1, 1))),
            Invalid::UnknownTable("v".into())
        );
        assert_eq!(
            invalid_of(u.set_schema("w", None)),
            Invalid::UnknownTable("w".into())
        );
        assert_eq!(u.finish().unwrap().root, base.root);

        // A row and an extra file may not share a path, whichever way round.
        let mut u = update();
        u.set_extra("t/a.json", Id::of(b"x")).unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::RowAndExtra("t/a.json".into())
        );
        let mut u = update();
        u.add(row("t/bad.json", 1, 1)).unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::RowAndExtra("t/bad.json".into())
        );
        // ... but a row may become an extra file in one delta, and back.
        let mut u = update();
        u.remove("t/a.json").unwrap();
        u.set_extra("t/a.json", Id::of(b"x")).unwrap();
        u.remove_extra("t/bad.json").unwrap();
        u.add(row("t/bad.json", 1, 1)).unwrap();
        u.finish().unwrap();
        let mut u = update();
        u.remove_extra("t/none.json").unwrap();
        assert_eq!(
            invalid_of(u.finish()),
            Invalid::NotPresent("t/none.json".into())
        );
    }

    /// Paths that land in one bucket, so leaves hold several rows.
    fn colliding(table: &str, n: usize) -> Vec<String> {
        let mut by_bucket: BTreeMap<(usize, usize), Vec<String>> = BTreeMap::new();
        let mut k = 0u64;
        loop {
            let path = format!("{table}/{k}.json");
            let slot = by_bucket.entry(bucket(&path)).or_default();
            slot.push(path);
            if slot.len() == n {
                return slot.clone();
            }
            k += 1;
        }
    }

    #[test]
    fn test8252_a_one_row_change_reads_and_writes_one_path_through_the_tree() {
        let mem = mem();
        let mut state = State::default();
        state
            .tables
            .insert("t".into(), Some(Hash256::of(b"schema")));
        for n in 0u32..2000 {
            state.rows.insert(
                format!("t/{n}.json"),
                (Hash256::of(&n.to_le_bytes()), Id::of(&n.to_le_bytes())),
            );
        }
        let base = build(&state);
        save(&mem, &base.objects);
        let counting = Counting {
            inner: &mem,
            reads: AtomicUsize::new(0),
        };
        let store = Store::open(&counting, Path::new(OBJECTS)).unwrap();
        counting.take();

        let mut update = TreeUpdate::new(&store, base.root).unwrap();
        update.modify(row("t/7.json", 9, 9)).unwrap();
        let built = update.finish().unwrap();
        assert_eq!(counting.take(), 4, "root, table, mid, leaf");
        assert_eq!(
            built.objects.len(),
            4,
            "a new leaf, mid, table and root; the rest is shared"
        );
        save(&mem, &built.objects);
        store.refresh().unwrap();
        assert_eq!(
            counting.take(),
            2,
            "a new pack costs its header and trailer"
        );

        assert_eq!(
            lookup(&store, &built.root, "t/7.json").unwrap(),
            Some((Hash256::of(&[9]), Id::of(&[9, 9])))
        );
        assert_eq!(counting.take(), 4, "one node per level");
        let changes = diff(&store, &base.root, &built.root).unwrap();
        assert_eq!(counting.take(), 8, "two of each level, nothing else");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "t/7.json");
        assert_eq!(diff(&store, &built.root, &built.root).unwrap(), vec![]);
        assert_eq!(
            counting.take(),
            0,
            "identical roots are compared by id alone"
        );
    }

    #[test]
    fn test8253_leaves_and_mids_appear_and_vanish_with_their_rows() {
        let fs = mem();
        let paths = colliding("t", 3);
        let mut state = State::default();
        state.tables.insert("t".into(), None);
        for (n, path) in paths.iter().enumerate() {
            state
                .rows
                .insert(path.clone(), (Hash256::of(&[n as u8]), Id::of(&[n as u8])));
        }
        let base = build(&state);
        save(&fs, &base.objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();
        let (mid, leaf) = bucket(&paths[0]);
        let leaf_len = |root: &Id| -> Option<usize> {
            let root = node::load_root(&store, root).unwrap();
            let table = node::load_table(&store, &root.tables[0]).unwrap();
            let mid_child = table.mids[mid]?;
            let leaf_child = node::load_mid(&store, &mid_child).unwrap().children[leaf]?;
            Some(
                node::load_leaf(&store, &leaf_child, "t", mid, leaf)
                    .unwrap()
                    .entries
                    .len(),
            )
        };
        assert_eq!(leaf_len(&base.root), Some(3));

        let mut update = TreeUpdate::new(&store, base.root).unwrap();
        update.remove(&paths[1]).unwrap();
        let fewer = update.finish().unwrap();
        save(&fs, &fewer.objects);
        assert_eq!(leaf_len(&fewer.root), Some(2));
        assert_eq!(
            lookup(&store, &fewer.root, &paths[0]).unwrap(),
            Some((Hash256::of(&[0]), Id::of(&[0])))
        );
        assert_eq!(lookup(&store, &fewer.root, &paths[1]).unwrap(), None);

        let mut update = TreeUpdate::new(&store, fewer.root).unwrap();
        update.remove(&paths[0]).unwrap();
        update.remove(&paths[2]).unwrap();
        let empty = update.finish().unwrap();
        save(&fs, &empty.objects);
        assert_eq!(
            leaf_len(&empty.root),
            None,
            "the empty leaf, and its mid, are gone"
        );
        let root = node::load_root(&store, &empty.root).unwrap();
        assert_eq!(root.tables[0].identity, empty_table_identity(None));
        let emptied = State {
            rows: BTreeMap::new(),
            ..state.clone()
        };
        assert_eq!(empty.root, build(&emptied).root);
    }

    #[test]
    fn test8254_tables_are_created_dropped_and_reschemaed_by_delta() {
        let fs = mem();
        let mut state = State::default();
        state.tables.insert("t".into(), None);
        state
            .rows
            .insert("t/a.json".into(), (Hash256::of(b"a"), Id::of(b"a")));
        let base = build(&state);
        save(&fs, &base.objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();

        let mut update = TreeUpdate::new(&store, base.root).unwrap();
        update.create_table("n", Some(Hash256::of(b"n"))).unwrap();
        update.add(row("n/x.json", 1, 1)).unwrap();
        update.set_schema("t", Some(Hash256::of(b"t2"))).unwrap();
        update.set_format(Some(Hash256::of(b"format")));
        let changed = update.finish().unwrap();
        save(&fs, &changed.objects);
        let mut expected = state.clone();
        expected.tables.insert("n".into(), Some(Hash256::of(b"n")));
        expected.tables.insert("t".into(), Some(Hash256::of(b"t2")));
        expected.format = Some(Hash256::of(b"format"));
        let x = row("n/x.json", 1, 1);
        expected.rows.insert(x.path.clone(), (x.row_hash, x.blob));
        let rebuilt = build(&expected);
        assert_eq!(
            (changed.root, changed.identity),
            (rebuilt.root, rebuilt.identity)
        );
        assert_ne!(changed.identity, base.identity);

        let mut update = TreeUpdate::new(&store, changed.root).unwrap();
        update.drop_table("n").unwrap();
        let dropped = update.finish().unwrap();
        save(&fs, &dropped.objects);
        assert_eq!(
            diff(&store, &changed.root, &dropped.root).unwrap(),
            vec![Change {
                path: "n/x.json".into(),
                before: Some((x.row_hash, x.blob)),
                after: None
            }]
        );
        // Dropping and re-creating a table is emptying it.
        let mut update = TreeUpdate::new(&store, dropped.root).unwrap();
        update.create_table("n", Some(Hash256::of(b"n"))).unwrap();
        let recreated = update.finish().unwrap();
        let mut update = TreeUpdate::new(&store, changed.root).unwrap();
        update.remove("n/x.json").unwrap();
        let emptied = update.finish().unwrap();
        assert_eq!(recreated.root, emptied.root);
    }

    #[test]
    fn test8255_an_inconsistent_tree_is_reported_not_believed() {
        let fs = mem();
        let path = "t/a.json";
        let (mid_index, leaf_index) = bucket(path);
        let leaf = Leaf {
            entries: vec![row(path, 1, 1)],
        };
        let mut objects = vec![];
        let mut node_object = |bytes: Vec<u8>| {
            let id = Id::of(&bytes);
            objects.push(Object::new(ObjectKind::Node, bytes));
            id
        };
        let leaf_id = node_object(leaf.encode());
        // A mid that records the wrong identity for its leaf.
        let mut lying = [None; FANOUT];
        lying[leaf_index] = Some(Child {
            id: leaf_id,
            identity: Hash256::of(b"not the leaf"),
        });
        // A mid that files the leaf under the wrong bucket.
        let mut misfiled = [None; FANOUT];
        misfiled[(leaf_index + 1) % FANOUT] = Some(Child {
            id: leaf_id,
            identity: leaf.identity(),
        });
        let mut roots = vec![];
        for children in [lying, misfiled] {
            let mid = Mid { children };
            let mid_child = Child {
                identity: mid.identity(),
                id: node_object(mid.encode()),
            };
            let mut table = TableNode {
                schema: None,
                mids: [None; FANOUT],
            };
            table.mids[mid_index] = Some(mid_child);
            let root = Root {
                format: None,
                config: None,
                extras: None,
                tables: vec![TableRef {
                    name: "t".into(),
                    identity: table.identity(),
                    id: node_object(table.encode()),
                }],
            };
            roots.push(node_object(root.encode()));
        }
        save(&fs, &objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();
        let error = lookup(&store, &roots[0], path).unwrap_err();
        assert!(
            matches!(error, StoreError::CorruptObject { id, what: Corruption::IdentityMismatch { .. } } if id == leaf_id),
            "{error:?}"
        );
        let error = entries(&store, &roots[1]).unwrap_err();
        assert!(
            matches!(&error, StoreError::CorruptObject { id, what: Corruption::Misplaced { path: p } } if *id == leaf_id && p == path),
            "{error:?}"
        );
        // A root id naming bytes that are no node, or a node of another kind.
        save(
            &fs,
            &[Object::new(ObjectKind::Blob, b"plain bytes".to_vec())],
        );
        assert!(matches!(
            entries(&store, &Id::of(b"plain bytes")).unwrap_err(),
            StoreError::CorruptObject {
                what: Corruption::BadMagic,
                ..
            }
        ));
        assert!(matches!(
            entries(&store, &leaf_id).unwrap_err(),
            StoreError::CorruptObject {
                what: Corruption::UnexpectedNode {
                    expected: "root",
                    found: "leaf"
                },
                ..
            }
        ));
        assert!(matches!(
            entries(&store, &Id::of(b"nowhere")).unwrap_err(),
            StoreError::Missing(_)
        ));
    }

    #[test]
    fn test8256_entries_are_in_path_order_across_tables_whose_names_prefix_each_other() {
        let fs = mem();
        let mut state = State::default();
        for table in ["a", "a-b", "a_b"] {
            state.tables.insert(table.into(), None);
            for n in 0u8..20 {
                state.rows.insert(
                    format!("{table}/{n}.json"),
                    (Hash256::of(&[n]), Id::of(&[n])),
                );
            }
        }
        let built = build(&state);
        save(&fs, &built.objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();
        let listed: Vec<String> = entries(&store, &built.root)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        let expected: Vec<String> = state.rows.keys().cloned().collect();
        assert_eq!(listed, expected);
        assert!(listed[0].starts_with("a-b/"), "`a-b/` sorts before `a/`");
    }

    #[test]
    fn test8257_lookup_refuses_paths_that_name_no_row() {
        let fs = mem();
        let built = build(&State::default());
        save(&fs, &built.objects);
        let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();
        for bad in ["", "t", "t/", "/t/a", "t/../a"] {
            assert_eq!(
                invalid_of(lookup(&store, &built.root, bad)),
                Invalid::Path(bad.into()),
                "{bad:?}"
            );
        }
        assert_eq!(lookup(&store, &built.root, "t/a.json").unwrap(), None);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Keep,
        Modify(u8, u8),
        Remove,
    }

    const TABLES: [&str; 4] = ["a", "a-b", "b", "users"];

    type Case = (
        State,
        Vec<Op>,
        Vec<(usize, String, u8, u8)>,
        Vec<Option<u8>>,
    );

    fn cases() -> impl Strategy<Value = Case> {
        let tables =
            prop::collection::btree_map(0usize..4, proptest::option::of(any::<u8>()), 1..4);
        let rows = prop::collection::btree_map(
            (0usize..4, "[a-z0-9]{1,3}"),
            (any::<u8>(), any::<u8>()),
            0..120,
        );
        (tables, rows).prop_flat_map(|(tables, rows)| {
            let mut state = State::default();
            for (index, schema) in &tables {
                state
                    .tables
                    .insert(TABLES[*index].into(), schema.map(|s| Hash256::of(&[s])));
            }
            for ((index, name), (r, b)) in rows {
                if tables.contains_key(&index) {
                    let path = format!("{}/{name}.json", TABLES[index]);
                    state
                        .rows
                        .insert(path, (Hash256::of(&[r]), Id::of(&[r, b])));
                }
            }
            let present: Vec<usize> = tables.keys().copied().collect();
            let ops = prop::collection::vec(
                prop_oneof![
                    2 => Just(Op::Keep),
                    1 => (any::<u8>(), any::<u8>()).prop_map(|(r, b)| Op::Modify(r, b)),
                    1 => Just(Op::Remove),
                ],
                state.rows.len(),
            );
            let adds = prop::collection::vec(
                (
                    prop::sample::select(present),
                    "[a-z0-9]{4}",
                    any::<u8>(),
                    any::<u8>(),
                ),
                0..40,
            );
            let extras = prop::collection::vec(proptest::option::of(any::<u8>()), 0..4);
            (Just(state), ops, adds, extras)
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 96, .. ProptestConfig::default() })]

        /// An updated tree is byte for byte the tree built from scratch; its
        /// diff is the naive difference; lookups and listings agree with a
        /// plain map.
        #[test]
        fn test8258_an_update_equals_a_rebuild_and_diff_equals_set_difference(
            (base, ops, adds, extras) in cases()
        ) {
            let fs = mem();
            let mut base = base;
            for (n, blob) in extras.iter().enumerate() {
                if let Some(blob) = blob {
                    base.extras.insert(format!("junk/x_{n}.txt"), Id::of(&[*blob]));
                }
            }
            let built = build(&base);
            save(&fs, &built.objects);
            let store = Store::open(&fs, Path::new(OBJECTS)).unwrap();

            let mut target = base.clone();
            let mut update = TreeUpdate::new(&store, built.root).unwrap();
            for ((path, _), op) in base.rows.iter().zip(&ops) {
                match op {
                    Op::Keep => {}
                    Op::Modify(r, b) => {
                        let entry = Entry {
                            path: path.clone(),
                            row_hash: Hash256::of(&[*r]),
                            blob: Id::of(&[*r, *b]),
                        };
                        target.rows.insert(path.clone(), (entry.row_hash, entry.blob));
                        update.modify(entry).unwrap();
                    }
                    Op::Remove => {
                        target.rows.remove(path);
                        update.remove(path).unwrap();
                    }
                }
            }
            for (table, name, r, b) in &adds {
                let path = format!("{}/{name}.json", TABLES[*table]);
                if base.rows.contains_key(&path) || target.rows.contains_key(&path) {
                    continue;
                }
                let entry = Entry {
                    path: path.clone(),
                    row_hash: Hash256::of(&[*r]),
                    blob: Id::of(&[*r, *b]),
                };
                target.rows.insert(path, (entry.row_hash, entry.blob));
                update.add(entry).unwrap();
            }
            if let Some(path) = base.extras.keys().next().cloned() {
                target.extras.remove(&path);
                update.remove_extra(&path).unwrap();
            }
            target.extras.insert("junk/new.txt".into(), Id::of(b"new"));
            update.set_extra("junk/new.txt", Id::of(b"new")).unwrap();

            let updated = update.finish().unwrap();
            let rebuilt = build(&target);
            prop_assert_eq!(updated.root, rebuilt.root);
            prop_assert_eq!(updated.identity, rebuilt.identity);
            let rebuilt_ids: HashSet<Id> = rebuilt.objects.iter().map(Object::id).collect();
            for object in &updated.objects {
                prop_assert!(rebuilt_ids.contains(&object.id()), "an update emits only nodes of the new tree");
            }
            save(&fs, &updated.objects);
            for id in &rebuilt_ids {
                prop_assert!(store.contains(id).unwrap(), "every node of the new tree is stored");
            }

            let listed: BTreeMap<String, (Hash256, Id)> = entries(&store, &updated.root)
                .unwrap()
                .into_iter()
                .map(|e| (e.path, (e.row_hash, e.blob)))
                .collect();
            prop_assert_eq!(&listed, &target.rows);
            let kept: BTreeMap<String, Id> = super::extras(&store, &updated.root).unwrap().into_iter().collect();
            prop_assert_eq!(&kept, &target.extras);

            let mut naive = vec![];
            let paths: BTreeSet<&String> = base.rows.keys().chain(target.rows.keys()).collect();
            for path in &paths {
                let (before, after) = (base.rows.get(*path).copied(), target.rows.get(*path).copied());
                if before != after {
                    naive.push(Change { path: (*path).clone(), before, after });
                }
            }
            prop_assert_eq!(diff(&store, &built.root, &updated.root).unwrap(), naive.clone());
            let reversed: Vec<Change> = naive
                .into_iter()
                .map(|c| Change { path: c.path, before: c.after, after: c.before })
                .collect();
            prop_assert_eq!(diff(&store, &updated.root, &built.root).unwrap(), reversed);

            for path in paths.iter().map(|p| p.as_str()).chain(["a/absent.json", "zzz/absent.json"]) {
                prop_assert_eq!(lookup(&store, &updated.root, path).unwrap(), target.rows.get(path).copied());
            }
        }

        /// Identity is a function of the logical state alone: not of the
        /// order rows were given in, and not of their blobs.
        #[test]
        fn test8259_identity_ignores_insertion_order_and_blobs(
            (state, _, _, _) in cases(),
            seed in any::<u64>(),
        ) {
            let built = build(&state);
            let mut rows: Vec<Entry> = state
                .rows
                .iter()
                .map(|(path, (row_hash, blob))| Entry { path: path.clone(), row_hash: *row_hash, blob: *blob })
                .collect();
            // A deterministic shuffle from the seed (xorshift).
            let mut s = seed | 1;
            for i in (1..rows.len()).rev() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                rows.swap(i, (s % (i as u64 + 1)) as usize);
            }
            let mut builder = TreeBuilder::new(state.format, state.config);
            for (name, schema) in state.tables.iter().rev() {
                builder.table(name, *schema).unwrap();
            }
            builder.extend(rows).unwrap();
            let shuffled = builder.finish().unwrap();
            prop_assert_eq!(shuffled.root, built.root);
            prop_assert_eq!(shuffled.identity, built.identity);

            let mut reblobbed = state.clone();
            for (_, blob) in reblobbed.rows.values_mut() {
                *blob = Id::of(&blob.0[..4]);
            }
            reblobbed.extras.insert("junk/x".into(), Id::of(b"x"));
            let other = build(&reblobbed);
            prop_assert_eq!(other.identity, built.identity);
            prop_assert_ne!(other.root, built.root, "the extras alone change the bytes");
        }
    }
}
