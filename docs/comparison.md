---
title: Comparison
---

# reldir beside other tools

reldir's premise is narrow: the data is a folder of JSON files that people and
programs read and edit directly, and it must nonetheless behave as a valid
relational database. Each tool below solves part of that problem, and several
are the better choice when the premise does not hold.

## SQLite, with JSON columns

SQLite stores data in one file in its own format, and its JSON functions query
JSON held in columns. It is faster than reldir at every operation, scales far
beyond it, and allows many readers and writers inside one process.

The data, though, is not files a person or a tool reads: an editor, `grep`
and `git diff` see a binary file. reldir uses SQLite itself -- its mirror is a
SQLite database and its SQL is SQLite's dialect -- but as a derived index over
the files rather than as their home.

**Choose SQLite** when the data's only readers are programs that go through
the database. **Choose reldir** when the files themselves are the interface.

## DuckDB, reading JSON files

DuckDB queries a folder of JSON in place (`read_json('users/*.json',
filename = true)`) with a columnar engine that is far faster than reldir for
analytics over large data, and reads Parquet, CSV and remote files as well.

It reads; it does not govern. Nothing checks that the files fit a schema, that
references resolve, or that a key is unique; a query over files an outside
edit broke answers from broken data. Writing back means producing files by
other means.

**Choose DuckDB** to analyse JSON files. **Choose reldir** to keep them valid,
and to change them. The two combine: `reldir export --as parquet` hands DuckDB
a validated table.

## Dolt

Dolt is a SQL database with git's model of history -- branches, merges, diffs
and clones of the data itself -- served over the MySQL protocol, and built for
far larger data and more writers than reldir.

Its data lives in Dolt's own storage (prolly trees), not in files you edit;
versioning is Dolt's, not your repository's. reldir's data is the files in an
ordinary git repository: git versions them, reviews them and merges them, and
reldir's merge driver makes a merge of two edits to one row file work member by
member (see [Git]({{ site.baseurl }}/git)).

**Choose Dolt** for a versioned database served to applications. **Choose
reldir** when the data should stay plain files in the repository your team
already works in.

## Git-backed content tools and flat-file stores

Static-site content managers keep content as Markdown or JSON files in a
repository and give editors a form-based interface; libraries such as lowdb
keep an application's data as JSON. Both keep data legible and versioned.

Neither is a relational database: cross-file references, uniqueness, rules
over sets of rows, SQL and multi-file transactions are left to the
application, and a change that breaks them is not refused.

**Choose them** for content edited through a form, or a small store one
program owns. **Choose reldir** when many files must stay mutually consistent.

## JSON files with a JSON Schema validator

Validating each file against a JSON Schema in CI -- with a validator such as
`ajv` or `check-jsonschema` -- keeps every file well formed with almost no
machinery.

A per-file validator sees one file at a time: it cannot say that `user_id`
names an existing user, that no two files hold the same key, or that a deletion
should take its references with it. It does not query, change or record.
reldir runs the same JSON Schema 2020-12 on every row and adds the rules that
span files, plus SQL, validated changes and history.

**Choose a validator** when each file stands alone. **Choose reldir** when the
files form a whole.

## Where reldir stops

reldir commits one change at a time, writes files for every change, and
examines the folder (or asks its monitor) whenever a process opens it. It is
built for up to around a million rows, edited by people, scripts and agents --
not for high write throughput, many concurrent writers, or data that no person
ever reads. See [Performance]({{ site.baseurl }}/performance) for what each operation costs.
