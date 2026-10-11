---
title: How to retry a change without applying it twice
---

# How to retry a change without applying it twice

This guide is for scripts, jobs and applications that change a database and
must cope with failure: a lock held by another writer, a killed process, a
disk that failed mid-commit. It shows how to make every retry safe, and how to
tell from the exit status whether to retry at all. Why the outcomes are what
they are is explained in [transactions]({{ site.baseurl }}/transactions).

## Name each change with an idempotency key

Give every change a key that names it -- the job and the unit of work it
does. If history already holds a revision made under that key, the change is
not made again, and that revision is reported:

```console
$ reldir --idempotency-key rename-ada-1 sql "UPDATE users SET name = 'Ada Lovelace' WHERE id = 'ada'"
action | path           | planned
-------+----------------+--------
write  | users/ada.json | false
(1 row)
committed: 1 file(s), revision #2 ([..])
$ reldir --idempotency-key rename-ada-1 sql "UPDATE users SET name = 'Ada Lovelace' WHERE id = 'ada'"
already made: revision #2 ([..]) carries this idempotency key; nothing was changed again
```

The replay exits `0`, and in machine output its `data.replayed` is `true`.
Every command that changes the database takes a key: SQL, the row commands,
`import`, `restore`, `migrate`, `schema new`, `schema pin`, `schema widen`,
`infer --write`, `doctor --fix` and `snapshot restore`. A retried migration is
not planned again, so one that dropped a column is reported, not refused
because the column is gone.

The key alone identifies the change. Any change under a key history already
holds is reported as made, whatever it says, so use a new key for every
distinct change, and the same key only to retry it:

```console
$ reldir --idempotency-key rename-ada-1 sql "DELETE FROM posts WHERE id = 'p2'"
already made: revision #2 ([..]) carries this idempotency key; nothing was changed again
```

From a library, pass the key with the change:

| Library | |
|---|---|
| [Python]({{ site.baseurl }}/python) | `with db.transaction(idempotency_key="transfer-42"):` |
| [Node.js]({{ site.baseurl }}/node) | `db.execute(sql, params, { idempotencyKey: 'withdraw-17' })`, or the same option to `db.transaction` |
| [Rust]({{ site.baseurl }}/rust) | `db.execute_with(sql, params, Change { idempotency_key: Some("transfer-42"), ..Default::default() })` |

A replay reports the earlier revision with `replayed: true`.

## Decide from the exit status

| Exit | The change | Do |
|---|---|---|
| `0` | was made, or was made before under its key | nothing |
| `3` | was not made: another writer, an outside edit or an expired transaction got in the way | retry it |
| `12` | was made and every reader sees it; finishing it on disk failed | do not retry; the next writing command, or `reldir recover`, finishes it |
| `13` | may or may not have been made | run `reldir recover`, then retry with the same key |
| `5`, `6` | was not made: an interrupted change must be finished, or metadata cannot be trusted | follow [How to recover]({{ site.baseurl }}/recover) |
| any other | was refused, and nothing was written | do not retry: the same change gets the same answer; fix the change or the data |

The codes behind exit `3`, each of which leaves nothing written:

| Code | A retry |
|---|---|
| `LOCK_CONTENDED` | waits for the lock again; to wait longer each time, pass `--wait SECONDS` |
| `CONCURRENT_MODIFICATION` | plans the change again against the files as they now are |
| `PLAN_CHANGED` | plans the change again; review it first if it was reviewed before |
| `TRANSACTION_EXPIRED` | starts the transaction over |
| `PATH_INTERFERENCE` | meets the same path: look at the path the error names first |

In a shell script:

```sh
key="nightly-import-$(date +%F)"
for attempt in 1 2 3 4 5; do
  reldir --idempotency-key "$key" sql -f import.sql
  status=$?
  case $status in
    0|12) exit 0 ;;                       # made; 12 is finished by the next writer
    3)    sleep "$attempt" ;;             # nothing written: try again
    13)   reldir recover || exit $? ;;    # then retry under the same key
    *)    exit $status ;;                 # refused: retrying cannot help
  esac
done
exit 3
```

In Python and Node.js, the error carries the same `code` and `exit`; see each
library's errors section.

## Commit only what was reviewed

When a person or a program reviews a change before it is made, commit it only
if the database is still in the state that was reviewed. A dry run reports
that state as `data.state`:

```console
$ reldir --format jsonl --dry-run sql "DELETE FROM posts WHERE id = 'p2'"
{"type":"change","action":"delete","path":"posts/p2.json","planned":true}
{"type":"result",[..],"data":{"files":1,"dry_run":true,"replayed":false,"state":"[..]"}}
```

Pass it back with `--expect-state`:

```sh
state=$(reldir --format json --dry-run sql "$change" | jq -r .data.state)
# ... review the plan ...
reldir --expect-state "$state" sql "$change"
```

If anything changed in between -- a row, a schema, a setting -- the change is
refused and nothing is written:

```console
$ reldir --expect-state 86e1d1baa8ea14071e7c9b46928d40f8 sql "DELETE FROM posts WHERE id = 'p2'"
error[PLAN_CHANGED]: the database changed since this change was planned, so the plan no longer says what committing would do; nothing was written
   = help: plan the change again, and review what it would do now
```

Plan it again, and review the new plan before committing it. The
[MCP server]({{ site.baseurl }}/mcp) holds every change an agent makes to this
rule: it plans first, and commits only the plan that was reviewed.
