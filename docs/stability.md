---
title: Stability
---

# Stability

What a program, a script or another tool may rely on changes incompatibly
only with the major version. Every other release may add to it or fix it.

## Version numbers

A version is `MAJOR.MINOR.PATCH`, and every commit has one, written to
`version.txt` as the commit is made:

- `MAJOR` is raised by hand, and only for a change to what this page
  governs. The current major version is 0.
- `MINOR` counts the commits in the repository's history.
- `PATCH` counts the changes since the last release of the same major
  version.

So the numbers of two releases always order them, newer above older, and two
releases with the same major version are compatible in everything below.
`reldir --version` names the version a binary was built from, and a release
is tagged `v` followed by it.

## What is governed

- **The on-disk format.** Format 3 is what every release of major version 0
  reads and writes. A folder written by one such release is read by every
  later one. A new format comes with a major release and a `reldir upgrade`
  that converts the old one, history included; the binary never interprets
  a format it does not know (`FORMAT_UNSUPPORTED`).
- **The schema dialect.** A schema valid under one release is valid, and
  means the same, under every later release of the same major version. A
  release may add a keyword or a column type; nothing is taken away.
- **The machine protocol.** Protocol 1 -- the result document, its record
  and event kinds, the diagnostic shape -- is what every release of
  major version 0 prints. A release may add a record kind, an event kind, or
  a member to `data`; a program reading the protocol ignores members it does not
  know. `reldir protocol schema` prints the protocol of the running
  release.
- **The error codes.** A code keeps its meaning and its exit status. A
  release may add codes; a code is never reused for another meaning.
- **The command line.** Commands, flags and their meanings are kept; a
  release may add them. Text written for a person (table output, help,
  messages) is not governed: programs read the machine protocol.
- **The libraries.** The public API of `reldir-core`, the Python package
  `reldir` and the Node package `@reldir/reldir` change incompatibly only
  with the major version. `cargo-semver-checks` compares `reldir-core`'s
  API on every pull request with the branch it merges into.

## Settings

`.db/config` refuses keys it does not know, so a typo is never silently
ignored. A release that adds a setting writes `"requires": "X.Y.Z"` into the
configuration it writes it to; an older release then refuses the file by
naming the release it needs, rather than calling the setting unknown.

## What is not governed

- Performance characteristics, beyond the targets the scale harness holds
  each release to.
- The derived files under `.db/` -- the mirror, the cache, the lock -- which
  any release may rebuild from the folder.
- Text meant for a person: wording of messages, help and table output.
- Anything a release documents as experimental. None is documented so.
