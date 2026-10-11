---
title: Stability
---

# Stability

reldir follows [semantic versioning](https://semver.org/). From 1.0, what a
program, a script or another tool may rely on changes only in a major
release; a minor release adds to it; a patch release fixes it.

## What is governed

- **The on-disk format.** Format 3 is what every 1.x release reads and
  writes. A folder written by one 1.x release is read by every later 1.x
  release. A new format comes with a major release and a `reldir upgrade`
  that converts the old one, history included; the binary never interprets
  a format it does not know (`FORMAT_UNSUPPORTED`).
- **The schema dialect.** A schema valid under one 1.x release is valid,
  and means the same, under every later one. A minor release may add a
  keyword or a column type; nothing is taken away.
- **The machine protocol.** Protocol 1 -- the result document, its record
  and event kinds, the diagnostic shape -- is what every 1.x release
  prints. A minor release may add a record kind, an event kind, or a member
  to `data`; a program reading the protocol ignores members it does not
  know. `reldir protocol schema` prints the protocol of the running
  release.
- **The error codes.** A code keeps its meaning and its exit status. A minor
  release may add codes; a code is never reused for another meaning.
- **The command line.** Commands, flags and their meanings are kept; a minor
  release may add them. Text written for a person (table output, help,
  messages) is not governed: programs read the machine protocol.
- **The libraries.** The public API of `reldir-core`, the Python package
  `reldir` and the Node package `@reldir/reldir` follow semantic
  versioning. `cargo-semver-checks` holds `reldir-core` to it on every
  change.

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
- Anything a release documents as experimental. 1.0 documents nothing so.
