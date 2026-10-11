---
title: Security
---

# Security

reldir reads folders that other people write: a repository you clone, a pull
request you review, a directory an agent was told to change. Its security
model follows from that. This page is about what reldir trusts, what it
treats as hostile, and why the lines fall where they do.

## Who is trusted, and what is not

reldir runs with the permissions of whoever runs it, and holds no secrets of
its own. It trusts two things:

- **the person or program running it**: its arguments, flags and environment,
  and the files it is told to import or export;
- **the operating system and filesystem**: that a rename is atomic, that a
  lock is held, that a synced file is on disk -- on the filesystems where
  reldir can verify those properties, as [below](#filesystems).

It does not trust **the folder**. A folder arrives from anywhere, and opening
one must be as safe as reading a file in an editor. So every row file,
schema, configuration file and history object is read as hostile input that
may be malformed, oversized, crafted to exhaust memory or the stack, or
arranged to make reldir write where it should not. Nothing in a folder can
make reldir run a program, reach the network, or read or write outside the
folder.

## Reading files strictly

A lenient parser is a security problem when two programs read one file. If a
row holds the same member twice, one reader may take the first value and
another the last, and the two disagree about what the row is -- a validator
and an application can be made to see different data. reldir refuses the
ambiguity instead of resolving it: a duplicate member name, at any depth, is
an error. So are a number that is not finite, content after the document, and
invalid UTF-8.

```console
$ echo '{"id": "eve", "name": "Eve", "name": "Mallory"}' > users/eve.json
$ reldir check
error[INVALID_JSON]: duplicate object key "name" at line 1 column 35
  --> users/eve.json:1:35
   |
 1 | {"id": "eve", "name": "Eve", "name": "Mallory"}
   |                                   ^
run `reldir doctor` for repairs, least destructive first
INVALID: 3 table(s), 8 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
$ rm users/eve.json
```

Size is bounded before content is trusted. A file larger than
`max_json_file_size` (64 MiB by default) is not read, and is reported as
`FILE_TOO_LARGE`; a document nested deeper than `max_nesting_depth` (128 by
default) is `NESTING_TOO_DEEP`, a limit enforced while parsing, so no document
can exhaust the stack. A bad file is reported on the file and line it is on,
and the rest of the database is still read and judged; an invalid database
answers queries only when asked with `--allow-invalid`, and reports its faults
first.

The kind of file matters as much as its contents. A row or schema must be a
regular file with one name. A symbolic link, a hard link, a socket, a device
or a FIFO where a row belongs is `NON_REGULAR_FILE`: it is never followed or
read, so a link planted in a repository cannot show the contents of a file
outside the folder as a row. reldir's own metadata files must likewise be
regular files with one name, or the database is not opened. Two row files
whose names differ only in case or Unicode normalization are reported
(`PORTABILITY_COLLISION`) -- as an error with `portable_names` -- because one
filesystem keeps them apart and another merges them, and a folder must not
mean two things on two machines.

## Schemas are data, not code

A schema can make validity depend on almost anything, which is why reldir
keeps it from depending on anything outside the database. A schema is checked
against reldir's [dialect]({{ site.baseurl }}/schemas#the-dialect) of JSON
Schema 2020-12 before it governs anything, and an unknown keyword, at any
depth, is an error.

- **No `$ref` leaves its document.** A reference to another file or a URL is
  `SCHEMA_REF_EXTERNAL`. Nothing is fetched, so validity never depends on a
  file the database does not govern, or on the network.
- **Checks and assertions are SQL that can only read.** They run under
  SQLite's authorizer, which admits reading the database's own tables and
  nothing else, and calling only the functions on a fixed list of
  deterministic built-ins -- never the clock, never randomness, never a
  function nobody examined. A rule that breaks this does not compile, as
  `SCHEMA_CHECK_INVALID` or `SCHEMA_ASSERTION_INVALID`. A rule that read the
  clock would make a valid database invalid tomorrow without a byte changing.
- **Rules are bounded** by the same limits as every query.

## SQL held to its permissions

A statement's text is a poor guide to what it does -- a common table
expression can hide a `DELETE`, a function name can be built at run time. So
every statement -- from a person, a script, an agent or a schema -- is held to
its permissions by SQLite's authorizer as it is compiled, not by inspecting its
text. A query may read the governed tables and the `json_each` and
`json_tree` functions. A mutation may also write the governed tables. Nothing
may read reldir's bookkeeping, attach a database, run a pragma, load an
extension, define or drop anything, or control a transaction:

```console
$ reldir sql 'SELECT name FROM sqlite_master'
error[QUERY_UNSUPPORTED]: the statement is refused: reading "sqlite_master", which is not a governed table
$ reldir sql "ATTACH 'other.db' AS other"
error[QUERY_UNSUPPORTED]: ATTACH is not supported: SQL here reads and changes rows; the structure of the database is its schemas
   = help: change a schema with `reldir migrate`, or by editing its file
```

A mutation never writes anything directly. It runs in a savepoint that is
rolled back; what it would have done becomes a plan of row changes, which goes
through referential actions and full validation before any file is written.
Parameters are bound as values, never spliced into the statement.

Every query is bounded -- in the rows it returns, the memory it uses, the time
it runs, and the bytes a change writes -- so no statement and no file can take
the machine. A bound that is reached refuses the answer with `RESOURCE_LIMIT`;
an answer is never silently truncated. The bounds are settings in
[`.db/config`]({{ site.baseurl }}/configuration#limits), which travels with the
folder, so a folder you did not write chooses its own: read its `.db/config`
before running anything over it, or set the bounds on the command line, which
overrides the file for one command.

## Writes that cannot be redirected

The dangerous thing a hostile folder could do is make reldir write somewhere
else. Every write goes through one path that makes a change whole or not at
all, and that never writes where it should not:

- A row's file name is spelled from its key by the table's naming rule:
  reldir's own rule percent-encodes what a name cannot hold, and a verbatim
  rule refuses a key that needs a character the folder's names never used. A
  path that would leave the root or reach into `.db/` is refused
  (`PATH_VIOLATION`), as is a name longer than the filesystem allows
  (`FILENAME_TOO_LONG`).
- A change is refused if a path it needs is not what it must be -- a symbolic
  link among its directories, a directory where a file goes
  (`PATH_INTERFERENCE`) -- so a link planted in the folder cannot redirect a
  write.
- A change is planned against the files as they were read, and written only if
  they are still that way; if another program changed one in between, nothing
  is written (`CONCURRENT_MODIFICATION`).
- Files are written to a temporary name, synced, and renamed into place, under
  the writer lock, with a journal that recovery finishes or undoes after a
  crash.

`--readonly` writes nothing at all: no rows, no history, no derived state.

### Filesystems

Atomic renames, locks and durable syncs are what make a change whole, and not
every filesystem provides them. reldir verifies the filesystem before writing,
and refuses to write on a network, FUSE, FAT or unrecognised filesystem
(`UNSAFE_FILESYSTEM`) unless the database's configuration sets
`allow_remote_filesystem` -- a decision for someone who knows that filesystem.
A table directory on another device than `.db/` is refused (`CROSS_DEVICE`),
because a rename cannot cross it.

## History is verified, not trusted

Recorded history is content-addressed: every revision and every object is
named by its hash, and is verified against it when it is read. An object that
does not match is `INTERNAL_METADATA_CORRUPT`, and reldir refuses to build on
it; `reldir check --deep` verifies every object history holds. A history merged
from another clone with [`--track-provenance`]({{ site.baseurl }}/git) is
checked the same way, so a clone's history is evidence, not testimony.

## Derived state

Some of `.db/` is reldir's own derived state -- the mirror (`.db/mirror.sqlite`)
that indexes the rows, and caches. It is rebuilt from the files whenever it is
missing, and `.db/.gitignore` keeps it out of version control, so a clone never
receives it. A folder received by other means -- an archive, a shared drive --
may carry derived state its sender made. Since all of it is derived, the safe
course costs only time: delete `.db/mirror.sqlite*` and `.db/cache/`, and
reldir rebuilds them from the files.

## The monitor

`reldir serve` watches the folder and tells other processes which files
changed. It listens on the loopback interface only, on a port chosen when it
starts. It publishes the port, with a random secret, in `.db/serve.json`,
created readable by the owner alone; every request must present the secret,
compared in constant time, so only a process that can read `.db/` may ask.

The monitor is an accelerator, never an authority. Before answering, it sees a
cookie file the asker writes, so its answer includes every change the asker
could have made or seen. No monitor, a monitor that does not answer, a token
from another of its runs, or a log it had to cut short all mean the folder is
examined whole, as it is without one. A monitor that misbehaved could make a
command slower, never wrong.

## Agents

An agent is a user that acts quickly and may misunderstand what it was asked.
[`reldir mcp`]({{ site.baseurl }}/mcp) gives it the same commands, under the
same validation, as a person, and adds what a person at a terminal provides
by watching:

- every change is planned first, and committed only against the state it was
  planned for (`PLAN_CHANGED` otherwise);
- a change that removes data -- a deleted row or file, a referential action --
  needs the user's own confirmation, asked through the client when it can
  ask, and decided from the plan, not from how the request was worded;
- tool arguments are checked against closed schemas, so a misspelt argument is
  refused rather than ignored;
- `--readonly` serves a session that cannot change anything.

The server acts with the permissions of the user who starts it. `import` reads
any file that user can read, and `export` writes a new file anywhere that user
can write -- never replacing one. What an agent can reach is therefore what
that user can reach, in the directory the server runs in; choose both with
that in mind.

## Git

Opening or checking a database runs no program. `reldir git setup` puts the
merge and diff drivers in the repository's local git config, which git never
takes from a clone; the committed `.gitattributes` names the drivers, but a
repository cannot make git run them on a machine where setup was not run. The
`--hooks` it installs run `reldir --readonly check`.
