---
title: MCP server
---

# MCP server

`reldir mcp` serves a database to AI agents over the
[Model Context Protocol](https://modelcontextprotocol.io). Every tool runs a
reldir command as the command line runs it, through the same validation, and
every change is planned before it is made. To connect a client, see
[How to give an AI agent access to a database]({{ site.baseurl }}/connect-an-agent).

## The server

```sh
reldir [GLOBAL FLAGS] mcp
```

The server speaks MCP on stdin and stdout, protocol version `2025-11-25`, and
names itself `reldir` with the binary's version. It offers tools, resources
with subscriptions, and prompts, and its `instructions` tell the agent how to
read and how to change the database.

The database is the one `--db` names, or the one found from the working
directory, as for every command (see [CLI]({{ site.baseurl }}/cli#finding-the-database)).
Global flags apply to every call of the session:

- `--readonly`: the session writes nothing at all -- no rows, no history, no
  derived state -- and every change tool answers `READ_ONLY`;
- `--timeout`, `--max-result-rows` and the other
  [limits]({{ site.baseurl }}/configuration) bound every query.

## Results

Every tool answers with the [machine protocol]({{ site.baseurl }}/protocol)'s
`result` document as structured content, and one line of text saying how the
call ended -- the summary, or the code and message of a failure. Each tool
declares that document as its output schema. A command that fails is a tool
result with `isError` set, which the agent reads like any other; a call the
server cannot run at all -- an unknown tool, arguments that do not fit -- is a
protocol error.

## Arguments

Each tool's input schema is closed: an argument it does not name, or one of the
wrong type, is refused before anything runs, as an `invalid_params` error that
lists every fault. A misspelt `dry_run` or `limt` is never silently ignored.

## Reading

| Tool | Arguments | Does |
|---|---|---|
| `status` | | validity, unrecorded changes, the revision |
| `tables` | | the tables, row counts and keys |
| `describe` | `table` | a table's columns, key, and references in and out |
| `schema_show` | `table` | a table's schema document |
| `get_row` | `table`, `key` | one row; a composite key is an array of values |
| `list_rows` | `table`, `where`, `order`, `limit` | a table's rows, filtered and ordered (`-column` for descending); 200 by default |
| `query` | `sql`, `params`, `limit`, `cursor`, `allow_invalid` | one read-only statement, a page at a time |
| `explain` | `sql`, `params` | SQLite's plan for a statement |
| `check` | `strict`, `deep` | every fault, with file, line and JSON Pointer |
| `lint` | `table` | how the schemas could say more |
| `infer` | `tables` | the schemas reldir would infer; writes nothing |
| `doctor_plan` | `only` | every repair doctor could make |
| `doctor_explain` | `fix` | what one repair does |
| `diff` | `table`, `from` and `to` (together), `git`, `schema_only` | what changed since the last revision, between two, or against a git revision |
| `log` | `limit` | recorded revisions, newest first |
| `show_revision` | `revision` | one revision and what it changed |
| `snapshot_list` | | the named snapshots |

Read tools are annotated read-only and idempotent.

### Pages and cursors

`query` answers a page of rows: 200 unless `limit` says otherwise, at most
10,000. When more remain, `data.more` is true and `data.next_cursor` continues
from where the page ended:

```json
{"sql": "SELECT id, title FROM posts ORDER BY id", "limit": 2}
{"sql": "SELECT id, title FROM posts ORDER BY id", "limit": 2, "cursor": "c1.2.<state>.<query>"}
```

A cursor belongs to one query -- its statement and values -- and to the state
the database was in when the first page was read. Passed with another query it
is refused (`USAGE`); once the database has changed, the next page could not
continue the same answer, so it is refused with `CURSOR_STALE`, and the agent
starts again from the first page. A page never mixes two states.

## Changing: plan, then commit

A tool that changes the database takes effect in two calls.

1. **Plan.** Called without `plan_id`, the tool validates the change against
   the database as it is and reports everything it would do -- every file it
   would write or delete, every referential action, the rows it would write --
   and writes nothing. Its result's `data` holds the plan:

   | Member | Holds |
   |---|---|
   | `plan_id` | the plan's identifier, to commit it with |
   | `state` | the state of the database the plan was made against |
   | `removes` | what it removes, one entry each: `deletes <path>` for a file it deletes, `<action> <path> (<constraint>)` for a referential action |
   | `overwrites` | how many existing files it rewrites |
   | `confirmation_required` | whether `removes` is not empty |

   The summary names the `plan_id` and says how to commit it.

2. **Commit.** Called again with the same arguments and the `plan_id`, the tool
   makes exactly that change -- provided the database is still in the state the
   plan was made against. The check is made under the writer lock, so nothing
   can slip in between. If anything changed, the commit is refused with
   `PLAN_CHANGED` and nothing is written; the agent plans again.

A plan commits once, only with the arguments it was made with; other arguments,
a plan already committed, or one from before the server started are
`PLAN_CHANGED`. The server keeps the 64 most recent plans.

### Confirmation

A plan that removes data -- deletes a row or file, or runs a referential
action -- says so in `data.removes`, and committing it needs the user's
confirmation. When the client supports elicitation, the server asks the user
directly, naming what the plan removes; declining is `DECISION_REQUIRED` and
nothing is written. When it does not, the agent must pass `confirm: true`,
which it should do only with the user's agreement. Rewrites of existing rows
are reported in `data.overwrites`; every rewritten row stays in history.

What a plan removes is decided from the plan itself, not from how the request
is spelt: a `DELETE` hidden in a `WITH` clause, or a cascade three tables away,
is a removal like any other.

| Tool | Arguments | Does |
|---|---|---|
| `mutate` | `sql`, `params` | one `INSERT`, `UPDATE` or `DELETE` |
| `transaction` | `statements` | several statements as one change; each sees what the ones before it did |
| `insert_row` | `table`, `row` | one row, or an array of rows |
| `update_row` | `table`, `key`, `patch` | set columns of one row |
| `delete_row` | `table`, `key` | delete one row, with its referrers' actions |
| `import` | `table`, `path`, `format`, `mode` | import a file on the server's machine (`insert`, `upsert` or `replace`) |
| `apply_fixes` | `only`, `allow_data` | apply doctor's repairs |
| `migrate` | `operations` | a migration: operations that reshape schemas and rows together |
| `schema_new` | `table` | declare a table |
| `schema_pin` | `table` | make an inferred schema the table's declaration |
| `schema_widen` | `target`, `null`, `optional`, `types`, `any` | widen a table's contract |
| `snapshot_restore` | `name` | make the database what a snapshot holds |
| `restore_rows` | `paths`, `at` | write row files back as history recorded them: a deleted row, or a row as it was |

Every change tool also takes `plan_id` and `confirm`. A `transaction`'s
statements are the transaction: `BEGIN`, `COMMIT` and `ROLLBACK` in them are
`TRANSACTION_STATEMENT`. Tools that can remove or rewrite data are annotated
destructive; none is open-world.

### Direct writes

Two tools write beside the database's rows, in one call:

| Tool | Arguments | Does |
|---|---|---|
| `snapshot_create` | `name` | keep the database as it is now under a name |
| `export` | `table`, `path`, `format`, `where`, `order` | write a table's rows to a new file -- JSON, JSON lines, CSV or Parquet -- which must not exist |

## Resources

| URI | Is |
|---|---|
| `reldir://dialect` | the JSON Schema dialect every table schema is written in |
| `reldir://protocol` | the JSON Schema of every tool result |
| `reldir://docs/<topic>` | a page of this documentation, as Markdown |
| `reldir://schema/{table}` | a table's schema document (a template) |
| `reldir://row/{table}/{key}` | one row, by its key (a template) |

A resource that does not exist -- an unknown table, a row no key names -- is a
`resource not found` error.

### Subscriptions

A client may subscribe to resources: with `resources/subscribe`, as every
client that opens with `initialize` (protocol 2025-11-25) does, or with a
`subscriptions/listen` stream, as later protocol revisions do. When the server commits a change, it notifies each
subscriber of every subscribed resource the change touched: a row resource for
a change to its table, a schema resource for a change to its schema. A change
to a schema also changes the list of resources, which subscribers of the list
hear. Changes made outside the server -- by another process, or by hand -- are
seen at the server's next call.

## Prompts

| Prompt | Arguments | Sets the agent to |
|---|---|---|
| `explore-database` | | learn the tables, how they relate, and what is wrong, changing nothing |
| `design-schema` | `table` (required), `purpose` | design or change a table through `schema_new` and `migrate`, showing each plan |
| `repair-database` | | find every fault with `check`, repair with `doctor_plan` and `apply_fixes`, confirming removals with the user |

## Cancellation

A client may cancel a call in flight. A query being run stops at its next
progress check, and the call ends without an answer. A change that has reached
its commit is not interrupted: it is made, or not made, whole.

## How it runs

Each call opens the database afresh, so it answers from the folder as it is --
including an edit another program made a moment ago -- and holds no lock
between calls. With [`reldir serve`]({{ site.baseurl }}/cli#serve) running, an
open reads only what changed since the last one. Commands run off the
protocol's thread, so a slow query does not stop the server from hearing a
cancellation.
