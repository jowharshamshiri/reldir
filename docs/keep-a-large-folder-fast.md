---
title: How to keep a large folder fast
---

# How to keep a large folder fast

This guide shows how to keep commands on a folder of tens of thousands of rows
or more answering in milliseconds. Why each step helps is explained in
[Performance]({{ site.baseurl }}/performance).

## Adopt the folder once, deliberately

The first command in a folder reads and judges every file. Make that command
`reldir init --adopt` (see
[How to put an existing folder of JSON under reldir]({{ site.baseurl }}/adopt-a-folder)),
run once, rather than letting the first query of a script or an agent pay for
it. Every command after it costs what changed.

## Run the folder monitor

Start `reldir serve` beside the database and leave it running:

```sh
reldir --db ./data serve
```

It runs until Ctrl-C or `SIGTERM`. On a server, run it under the service
manager, so it starts with the machine and restarts if it stops. With
systemd, as a user service:

```ini
# ~/.config/systemd/user/reldir-data.service
[Unit]
Description=reldir folder monitor for /srv/data

[Service]
ExecStart=/usr/local/bin/reldir --db /srv/data serve
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now reldir-data.service
```

To confirm that commands use it, ask for the status as JSON and look at
`monitored`:

```sh
reldir --db ./data --json status
```

```text
    "monitored": true,
```

If it reads `false` while the monitor runs, that command examined the folder
whole; [Performance]({{ site.baseurl }}/performance#why-a-monitor-and-why-it-never-decides)
lists when that happens. A `--readonly` command never uses the monitor.

## Keep `.db/` on a local disk, beside the rows

Changes are staged in `.db/` and renamed into place, so `.db/` must be on the
same filesystem as the table directories (`CROSS_DEVICE` otherwise). On a
network, cluster, FUSE, FAT or shared-folder filesystem, reldir refuses to
write (`UNSAFE_FILESYSTEM`; the
[list]({{ site.baseurl }}/configuration#files)):

- move the folder to a local disk; or
- if you know the filesystem's locks, renames and syncs are reliable, set
  `allow_remote_filesystem` in
  [`.db/config`]({{ site.baseurl }}/configuration#files).

## Index the columns you filter on

Keys, unique constraints and the source columns of foreign keys are indexed
already. For another column that queries filter or sort on, check the plan:

```console
$ reldir explain "SELECT count(*) FROM posts WHERE title = 'Engines'"
id | parent | detail
---+--------+-----------
3  | 0      | SCAN posts
(1 row)
1 plan step(s)
```

`SCAN` reads every row. Add an index:

```console
$ reldir migrate add-index posts title
description
------------------
index posts(title)
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/posts.json | false
(1 row)
migrated: 1 file(s), revision #2 ([..])
$ reldir explain "SELECT count(*) FROM posts WHERE title = 'Engines'"
id | parent | detail
---+--------+---------------------------------------------------------------------[..]
3  | 0      | SEARCH posts USING COVERING INDEX [..] (title=?)
(1 row)
1 plan step(s)
```

The index is declared in the table's schema, in `x-reldir.indexes`, so every
clone has it.

## Refresh the planner's statistics after a bulk change

Statistics are refreshed when a table doubles or halves in size. When a bulk
change alters what a table holds without changing its size much -- a
re-import, a mass update of a filtered column -- and queries slow down,
refresh them:

```console
$ reldir analyze
refreshed query statistics
```

## Change many rows in one command

Each command opens the database, and each commit flushes to disk. Put many
changes in one statement:

```console
$ reldir sql "UPDATE posts SET title = title || '.' WHERE user_id IN ('ada', 'grace')"
action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
write  | posts/p2.json | false
(2 rows)
committed: 2 file(s), revision #3 ([..])
```

When the changes differ row by row, use one script -- statements separated by
`;`, or a file given with `reldir sql -f FILE` -- which commits once:

```console
$ reldir sql "UPDATE users SET name = upper(name) WHERE id = 'ada'; UPDATE users SET name = upper(name) WHERE id = 'grace'"
action | path             | planned
-------+------------------+--------
write  | users/ada.json   | false
write  | users/grace.json | false
(2 rows)
committed: 2 file(s), revision #4 ([..])
```

For rows that come from elsewhere, use `reldir import` (see
[How to import and export rows in bulk]({{ site.baseurl }}/import-and-export)).
From Python, `executemany` is one transaction; from Node and Rust, use a
transaction.

## Bound what a query may take

A query over a large folder can sort or return a large part of it. Bound
queries run by scripts and agents with `--timeout`, `--max-result-rows` and
`--max-query-memory`, or for every command with the matching settings in
[`.db/config`]({{ site.baseurl }}/configuration#limits). A query past a bound
ends with `RESOURCE_LIMIT` rather than a partial answer.

## Measure your machine

From a clone of the repository, the scale run builds a folder of the size you
name, times every operation in [Performance]({{ site.baseurl }}/performance#at-scale)
against its target, and fails on a miss:

```sh
cargo build --release -p reldir
cargo xtask scale --rows 100000
```
