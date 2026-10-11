---
title: Performance
---

# Performance

A database server owns its data: nothing changes it except through the
server, so the server always knows what it holds. reldir owns nothing. The
files are the database, and an editor, a script or `git checkout` may change
any of them between two commands. Every command must therefore begin by
finding out what the folder holds now, and the central problem of reldir's
performance is making that cost what changed rather than what exists. This
page is about how it does, and where the costs remain.
[How to keep a large folder fast]({{ site.baseurl }}/keep-a-large-folder-fast)
puts it into practice.

## Trusting a file's stat

The cheapest proof that a file has not changed is its stat. The
[mirror]({{ site.baseurl }}/format#the-mirror), `.db/mirror.sqlite`, keeps for
every file the stat it had when last read -- size, modification and change
times, inode and device -- beside its parsed row and its row-local verdict. A
file whose stat is unchanged is neither read nor judged again, so opening a
database costs one `stat` per file, and a read only of each file that changed.

A stat is not a perfect proof. A file written twice within one tick of the
filesystem's clock keeps the same modification time, and a reader that looked
between the two writes would trust a stale record. This is the problem git
calls "racy" timestamps, and reldir answers it the way git does: it measures
the filesystem's timestamp granularity once, and reads again any file whose
modification time falls within one tick of the moment it was last seen. That
moment is taken from a marker file in `.db/cache/`, so it is measured by the
filesystem's own clock, not the machine's. The price is an occasional
needless read of a file that was just written; the alternative is an answer
that can be wrong.

When the observation finds nothing changed -- no file read afresh or gone, and
the same schemas, structure and settings -- the database is not judged again:
the verdict reached last time is the verdict now. An unchanged database costs
its stats and nothing more.

## Why a monitor, and why it never decides

A `stat` is fast, but 100,000 of them are not instant, and most of them confirm
that nothing happened. `reldir serve` removes them. It watches the folder
through the operating system's change notifications (inotify, FSEvents,
ReadDirectoryChangesW) and keeps a numbered log of every path that changes. A
process opening the database -- the command, the
[MCP server]({{ site.baseurl }}/mcp), the [Python]({{ site.baseurl }}/python)
and [Node]({{ site.baseurl }}/node) libraries -- asks it which paths changed
since the mark its mirror stored, and examines those alone.

Change notifications arrive late and in no promised order, so "what changed
since my mark" is a question with a race in it. The monitor speaks git's
fsmonitor protocol, which closes the race with a cookie: before asking, the
process writes a cookie file in `.db/cache/`, and the monitor answers only
once it has seen that file appear. Every change made before the question was
asked is then in the answer.

The monitor accelerates; it never decides. A monitor that could be wrong would
make every answer depend on a process nobody is watching, so any doubt sends
reldir back to examining the folder whole, exactly as it does without one: no
monitor, a monitor that does not answer within two seconds, a mark from
another of its runs, a log it had to cut short (it keeps the last 1,048,576
paths), or a notification the operating system dropped. A `--readonly`
command keeps no mirror of its own, so it always examines the folder whole.
The monitor's own exposure is small -- loopback only, behind a secret; see
[security]({{ site.baseurl }}/security#the-monitor).

## What a change costs

A change could be judged by validating the whole database it would produce,
which is simple and correct and costs the whole database. reldir judges a
change by its scope instead. It is planned, completed with its referential
actions and judged from memory, laid over the files without copying anything:
the rows it writes and removes, the keys they held and hold, and the
references into and out of them are checked against indexed queries over the
mirror, and every finding the change could not have affected is carried over
from the verdict before it. Past 4,096 row files, judging the scope costs more
than judging everything, and the change is judged whole once instead.

Once written, the state the change was judged as is the new state: it is
kept, not observed again. Which rows differ from the last revision is tracked
as they change, so recording the revision costs the rows that changed.

What dominates a write is not computation but flushing to disk, and a flush
per file would make a 10,000-row change take minutes. Every new file is
therefore staged and flushed together, by up to 16 threads; on Linux 5.8 and
later a change of 256 files or more is flushed with one `syncfs`, which
commits the filesystem's journal once. Then one journal is flushed, and the
files are renamed into place. [Transactions]({{ site.baseurl }}/transactions)
describes why that order makes the change whole.

## Queries

Queries run on the mirror, which SQLite indexes: every primary key, unique
constraint, foreign key's source columns and column list declared in
`x-reldir.indexes`. Lookups by key and joins along references are indexed
without being asked for. Correctness never depends on an index, only speed.

The query planner chooses well only with statistics that describe the data.
They are gathered when a table is first mirrored and again whenever it has
doubled or halved in size since (counting any table under 64 rows as 64), so
growth keeps them current; `reldir analyze` gathers them for every table at
once.

A reading command does not wait for a writer. If the writer lock is free, it
takes it just long enough to observe the files, then runs the query on that
observation holding nothing. If a writer holds it, the query answers at once
from the database as of that writer's last commit, with the warning
`SNAPSHOT_READ`. A slow write therefore never stalls a read.

Queries are bounded rather than best-effort: past `max_result_rows`,
`max_query_memory` or `timeout_seconds` a query ends with `RESOURCE_LIMIT`.
A truncated answer that looked whole would be worse than none; see
[configuration]({{ site.baseurl }}/configuration#limits).

## History

History could copy every row at every revision; it shares instead. Recording
a revision writes only the tree nodes on the paths of the rows that changed --
a leaf, a mid, a table and the root -- and the bytes of the files that
changed; everything else is shared with the revision before. `reldir diff`
between two revisions descends only into the parts of the trees that differ.
A snapshot of the recorded, valid state is a name for a tree history already
holds, and costs one small file.

Each commit publishes one pack of objects. Many small packs make reads slow,
and rewriting one large pack at every commit makes writes slow; once there are
more than 16 packs, the smallest are merged, so the packs form a geometric
progression and a large pack is rewritten rarely. `reldir gc` removes what
nothing reaches. The [format]({{ site.baseurl }}/format#history) gives the
details.

## At scale

The repository's scale run builds a folder of `users` and `posts` (two posts
per user, each referencing its author), runs the release binary over it, and
fails if an operation misses its target. It runs every week in continuous
integration on 100,000 rows:

| Operation, on 100,000 rows | Target |
|---|---|
| `status`, no monitor (a `stat` of every file) | 1 s |
| `status`, monitored | 100 ms |
| `SELECT count(*)`, monitored | 100 ms |
| `SELECT` by an indexed key, monitored | 100 ms |
| one-row `UPDATE`, monitored | 150 ms |
| 10,000-row `UPDATE` | 3 s |
| `snapshot create` | 500 ms |
| `check`, monitored | 1.5 s |
| `.db/` on disk, against the rows on disk | at most 1.5× |

Targets for the whole-folder operations scale linearly above 100,000 rows.

## Where the costs remain

Some costs follow from the premise and do not go away. The first command in a
folder adopts it, reading and judging every file once. Without the monitor,
every command costs a `stat` of every file. Every command opens the database
afresh, and every commit flushes at least once, so a thousand one-row commands
cost far more than one statement that changes a thousand rows. A network
filesystem cannot promise the renames and locks a commit relies on, so reldir
refuses to write on one unless told otherwise. reldir is built for up to
around a million rows edited by people, scripts and agents; beyond that, or
for many concurrent writers, a database that owns its data is the better tool
-- see the [comparison]({{ site.baseurl }}/comparison).
