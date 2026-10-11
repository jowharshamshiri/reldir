---
title: Concepts
---

# Concepts

This page is about the ideas reldir is built on: what it means for a folder
of files to be a database, what reldir promises the programs that read those
files, and why it works the way it does. The commands, keywords and codes
themselves are described in the reference pages it links to.

## The folder is the database

A table is a directory, a row is one JSON object in one file, and a table's
rules are a JSON Schema document. There is no second copy of the data that
the files merely reflect. Everything reldir keeps beside them, in `.db/`, is
either the history of the files or state derived from them that it can
rebuild:

```text
database/
├── schema/          schemas people declared
├── users/           one file per row
│   ├── ada.json
│   └── grace.json
└── .db/             format, settings, inferred schemas, history, the mirror
```

Most databases make the opposite choice, and for good reasons: a storage
engine that owns its files can lay them out for speed, change many records in
one write, and never wonder whether someone else touched them. reldir gives
that up deliberately. Data that people review, diff, merge and edit with
ordinary tools -- configuration, content, reference data, curricula, test
fixtures -- is most useful as plain files. Such data usually has no guardian:
nothing stops a script from writing a reference to a row that does not exist,
or a typo in a member name that every reader then silently ignores. reldir is
that guardian, without taking the files away from anyone.

The cost is that the files can change at any moment, behind reldir's back. So
every command begins by finding out what the folder holds now, and judges it
before it answers or changes anything. Much of reldir's design -- the mirror,
the stat cache, the folder monitor -- exists to make that first step cost what
changed rather than what exists. The [storage format]({{ site.baseurl }}/format)
describes `.db/` file by file.

## The observed contract

A folder of JSON that other programs read already has a schema, though nobody
wrote it down. A program that reads `users/*.json` relies on every user having
a `name`, on `name` being a string and never null, on there being no members it
does not know about, on `age` being written as an integer. That shape is a
contract between the folder and every program that reads it, whether or not
anyone stated it. reldir calls it the table's *contract*, and makes one promise
about it: **nothing reldir writes on its own authority takes a file outside its
contract.** Inserts, updates, deletes and the referential actions they trigger,
imports and repairs all keep it.

### Shape, not coincidence

When reldir adopts a folder, it writes down the contract it observes. The
difficulty is deciding what counts as the contract and what is merely true of
today's data. If every user in the folder happens to have a distinct name, is
the name unique? If every status is one of three strings, is that an
enumeration? If every price is positive, is that a rule?

reldir's answer is that the contract is the *shape* of the files and nothing
more: which members exist, which are always present, which have ever been
null, what type each value has and how it is written. These are the facts a
reading program depends on to parse a file at all. The rest are
coincidences of the data. Stating them as rules would make an ordinary insert
fail because nobody has yet happened to repeat a name, or because a fourth
status had not been needed yet. So inference states shape and stops there, and
`reldir lint` proposes the rest as suggestions a person can adopt on purpose.

The same reasoning runs the other way. A member that has only ever been null
is recorded as admitting only null, not anything at all; a member that is
sometimes absent but never null is optional, not nullable. "Unobserved" means
"not promised". A contract that admitted more than the files showed would let
reldir write values the reading programs have never seen.

### Widening is a decision

Sooner or later a change needs a value the contract has never admitted: the
first user without an email, the first price written as a string. reldir
refuses such a change with `CONTRACT_WIDENING_REQUIRED` and names the widening
that would admit it. It does not widen on its own, because it cannot know
whether the programs reading the folder will cope: a widening is a change to
an agreement with programs reldir has never met. Only a person can know that,
so a widening is always explicit -- on the change itself, as a command of its
own, as a migration or as an edit to the schema -- and each is recorded in
history as a decision someone made.

### Outside edits are warned, not refused

A program that writes the files directly may write a value beyond the
contract. reldir did not make that edit and cannot undo it without destroying
someone's data, so it does not treat it as corruption. The value is reported
as a `CONTRACT_EXCEEDED` warning, the file is left exactly as it is, and the
contract is not widened behind anyone's back. reldir also never writes a value
like it until someone widens the contract, so one program's habit does not
spread through reldir to every file.

### Pinned and inferred

A contract reldir inferred describes what the folder *was*. A schema a person
wrote down -- a *pin*, in `schema/` -- describes what the folder *must be*. The
two are treated differently for that reason: in a pinned table, a value
outside the schema is an error whoever wrote it, because someone has declared
the rule. Pinning is how a table moves from "what we observed" to "what we
decided", and it is usually the moment a table starts to be designed rather
than discovered.

The inferred contracts are kept in version control with the rows, so every
clone of a repository holds the same contract rather than each one inferring
its own from files that may have drifted.

The rules inference follows, and every way to widen, are listed in the
[schema reference]({{ site.baseurl }}/schemas#contracts).

## Identity

Every row has a key, and every key names exactly one file. Where the key comes
from is part of the table's schema. In some folders the key is in the body --
`"id": "ada"` in `users/ada.json` -- and the file name spells it. In others the
file name *is* the key, and the body never repeats it: `people/ada.json` is the
row whose key is `ada`. reldir treats both as first-class, and in the second
case shows the file name to SQL as a column like any other.

When reldir adopts a folder, it learns the naming convention the files already
follow instead of imposing its own. Renaming files would break every program
and link that names them, and the promise not to change the folder's contract
covers file names too. So a folder whose names are plain lowercase words keeps
plain lowercase words; one whose names wrap a number in fixed text, as in
`user-17.json`, keeps that. Where a new key cannot be spelled in the folder's
convention -- a space, in a folder whose names have never had one -- reldir
refuses the key rather than invent an escaping scheme the folder never used.
A table reldir creates uses reldir's own naming rule, which percent-encodes
whatever a plain name cannot hold, so there every key has a name.

Names that differ only by case or Unicode form are two rows to reldir, but one
file to some filesystems. A folder that only lives on Linux can use them; one
that also moves through macOS or Windows cannot, and the `portable_names`
setting turns the warning into an error for such folders.

## Writing only what changes

When reldir changes a row, it edits the file rather than replacing it. Every
member the change does not touch keeps its exact bytes: a number written
`1.50` stays `1.50`, an integer beyond 64 bits keeps every digit, a timestamp
keeps its offset, a string keeps its Unicode form, the members keep their
order, and the file keeps its indentation and line endings. A change that
leaves a value as it was writes nothing at all.

This matters for two reasons. The first is the other programs: a value they
wrote in a form they rely on must not come back in a form they do not. A
JSON library that parses `12345678901234567890123` as a float, or normalises a
timestamp to UTC, would silently change data no one asked it to change. The
second is people: a one-member change should be a one-line diff in a pull
request, not a file rewritten from top to bottom. The written form is what
other programs read, what names files and what reviewers see, so reldir's
column types state how a value is written, not only what it is.

New files are written in the style the table's files already share, measured
when the folder was adopted, so a row reldir creates looks like its
neighbours.

## Validity

Every command begins by judging the folder: every schema, every row, every
key, every reference and every rule over many rows. The database is valid when
there is nothing to report, and then the state is recorded in history if it is
new -- a valid edit made outside reldir becomes a revision of its own.

A database can be invalid, because anything may edit the folder. reldir's
response is asymmetric. Commands that look at the database, or that exist to
make it valid again -- `status`, `check`, `doctor`, `log`, `diff`, `recover`,
restoring a snapshot -- still work. Queries refuse unless asked to answer
anyway, because an answer computed over broken references may mislead. And
changes refuse outright: a change is judged by whether the state it makes is
valid, and against a state that already has faults, that question has no
honest answer. reldir would either blame the change for faults it did not
cause, or let it add new ones among them.

A row deleted by hand is not cascaded after the fact. reldir did not delete
it, so it does not know whether a cascade is what anyone wanted; the reference
to the missing row is reported as a fault, and `reldir doctor` offers the
repairs, least destructive first -- restoring the row from history before
deleting anything.

## The mirror

SQL does not run on the files. It runs on a mirror of them, a SQLite database
in `.db/` with one SQL table per table, every key and every reference indexed,
and each file's stat, hash and verdict beside its row. A query that joins two
tables needs indexes and a query planner; a folder of files has neither.

The mirror is never the authority. It is derived from the files, kept current
incrementally -- a file whose stat has not changed is not read again -- and
rebuilt from them whenever it is missing. Deleting it changes how fast the
next command is, never what it answers. That is what lets the files remain the
source of truth while queries run at the speed of an indexed database.

## Readers and writers

One writer at a time holds the writer lock, from observing the folder until
its change is committed. Other writers queue for it, and each plans against
the state it will commit to, so two processes incrementing the same counter
never lose an update.

Readers do not queue. While a writer holds the lock, a read answers from the
database as of the last commit, says so, and never sees half of a change.
Making readers wait would let one slow import stall every dashboard and agent
reading the folder; answering from the last commit costs nothing in
correctness, because the half-written change is not yet part of any valid
state. How a change is committed, and what happens if the machine stops
halfway, is explained in [Transactions]({{ site.baseurl }}/transactions).

## History

Every state the database is accepted in is a *revision*: a record of every
row and schema at that moment, the revision or revisions before it, when it
happened, and how -- adopted, changed by a person, edited outside reldir,
repaired, migrated, imported, restored, merged. Revisions, the trees of their
states and the files' bytes are stored by the hash of their content. A row
that did not change between two revisions is stored once, and any recorded
row can be restored byte for byte.

Formatting is not state. A file re-indented without a change of value is the
same state, so the history of a table records changes of meaning, not of
whitespace.

Content addressing also makes history safe to share. Two clones that each
record revisions produce objects with different names, never two files with
the same name and different contents, so histories kept in version control
merge without conflict. A merged history has two newest revisions, and the
next one recorded joins them. History that fails to verify is not repaired
by guessing: it is moved aside intact, and a new lineage begins from the
files as they are.

A change can carry an *idempotency key*, so that a program retrying after an
uncertain failure -- a crash, a lost connection -- learns that the change was
made instead of making it twice.

The commands and the record format are described in
[History and snapshots]({{ site.baseurl }}/snapshots).

## The folder monitor

Finding out what a folder holds costs one `stat` of every file: cheap per
file, but not for a hundred thousand of them. `reldir serve` watches the
folder with the operating system's change notifications and keeps a log of
what changed. Every process that opens the database -- the command, the MCP
server, the libraries -- asks it which paths changed and examines only those.

The monitor is an accelerator, not a dependency. Without it, every command
examines the whole folder, which is always correct and only slower. reldir
never trusts the monitor further than the operating system's notifications
can be trusted: when the log might be incomplete, the next command examines
the folder whole. See [Performance]({{ site.baseurl }}/performance).

## One way to change anything

Every change follows one path: plan the row changes, complete them with their
referential actions, judge the state they produce, check it against the
contract, and commit it as one revision. SQL statements, the row commands,
imports, migrations, repairs and snapshot restores all take it, and nothing
writes a row any other way.

One path means one set of guarantees. A repair cannot leave a dangling
reference that a SQL statement would have refused; an import cannot write a
value the contract never admitted; a migration is validated as thoroughly as an
insert. Each new way to change the data inherits every rule instead of
re-implementing them.

## Nothing is silently absorbed

reldir reports everything in the folder it does not recognise as a row: a file
that is not JSON in a table's directory, a directory no schema governs, a
symlink where a row belongs, a file holding a merge's conflict markers. Only
editor artefacts and paths listed in the configuration are ignored. A folder
whose stray files were quietly skipped would let a typo in a file name, or a
half-finished merge, become invisible state.

## What reldir is not

It is not a server, and not built for many concurrent writers: one writer
commits at a time, and every change writes files. It is not a replacement for
a database whose data no person reads. It is for data that people and tools
should be able to read, review, diff and edit directly, and that must
nonetheless always be right -- up to around a million rows. How it compares
with other tools is discussed in [Comparison]({{ site.baseurl }}/comparison).
