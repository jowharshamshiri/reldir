---
title: Configuration
---

# Configuration

`.db/config` holds the settings a person chooses. It is versioned beside the
rows, so every clone formats and limits alike. An unknown key is an error, not
something to ignore (`CONFIG_INVALID`), and every setting is checked before it
is used.

## Settings

`reldir init` writes the defaults:

| Key | Default | Meaning |
|---|---|---|
| `indentation_width` | `2` | spaces per level when reldir writes a file |
| `enum_max_values` | `10` | most distinct values a string column may hold and still be inferred as an `enum` |
| `unique_min_rows` | `20` | rows needed before inference or lint proposes a `unique` constraint or a check |
| `max_json_file_size` | `67108864` | largest file read, in bytes |
| `max_nesting_depth` | `128` | deepest JSON nesting read |
| `max_result_rows` | `1000000` | most rows a query may return |
| `max_query_memory` | `268435456` | bytes a query may allocate, sorting included |
| `max_transaction_size` | `1073741824` | most bytes one change may write |
| `timeout_seconds` | `null` | seconds a query may run; `null` for no limit |
| `wait_seconds` | `5.0` | seconds a writer waits for the lock; `0` tries once |
| `ignore` | `.DS_Store`, `*~`, `*.swp`, `.gitkeep` | glob patterns reldir does not govern |
| `allow_remote_filesystem` | `false` | permit writing on a network or FUSE filesystem ([why]({{ site.baseurl }}/transactions#filesystems)) |
| `reference_naming` | see below | the column names that announce a reference |
| `reference_min_values` | `1` | distinct resolving values a place must hold before a reference is proposed |

Every limit must be greater than zero; `wait_seconds` may be zero and must be
finite. Exceeding a limit is always `RESOURCE_LIMIT`, never a truncated answer.

## Reference naming

Inference declares a reference only when a place is *named* as one to its
target; lint proposes the others. The names are patterns, with `{table}`
standing for a table's name and `{singular}` for it without a trailing `s`:

```json
{
  "reference_naming": [
    "{singular}_id", "{table}_id", "{singular}_ids",
    "{singular}_ref", "{singular}_refs", "{table}_ref", "{table}_refs",
    "{singular}", "{table}"
  ]
}
```

With these, `user_id`, `user_ids`, `lesson_ref` and `objective_refs` announce
references to `users`, `lessons` and `objectives`. A pattern must name the table
with a placeholder, or it would match every table.

## For one command

Each limit can be overridden for a single command:

```sh
reldir --max-result-rows 50 sql 'SELECT * FROM events'
reldir --max-nesting-depth 512 check
reldir --timeout 30 sql 'SELECT ...'
reldir --wait 0 update users u1 '{"name":"Ada"}'
```

## Version control

`.db/.gitignore` versions `format` and `config` and ignores the rest -- the
mirror, working schemas, history, snapshots and transaction staging are
derived, per-clone or ephemeral. After a `git clone` the first command reads
every row, builds the mirror and records the first revision.

`reldir init --track-provenance` versions history as well: `provenance/` and
the `objects/` it references, so a clone carries a complete, verifiable
history.

In CI, `reldir --readonly check --format sarif` writes nothing, exits `2` on an
invalid database, and produces a log code-scanning tools can annotate. A merge
that leaves conflict markers in a row is `INVALID_JSON`, and the diagnostic
says so.
