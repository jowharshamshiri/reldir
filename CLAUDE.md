# reldir

reldir governs a folder of JSON files, one directory per table and one file per row, as a relational database. This file holds what a session working on reldir needs beyond the code and `docs/`.

## The observed contract

A folder of JSON that other programs read has a contract nobody wrote down: its shape. Without the user's explicit say-so, nothing reldir writes on its own authority may take a file outside that contract: not `init`, not any insert, update or delete, not the referential actions they trigger, not imports or repairs. This is the product's central promise; `docs/concepts.md` explains it.

- The contract is shape only: which members exist, which are always present, which have been null, what type each value has and how it is written. Coincidences of today's data (distinct values, enumerations, bounds) are never rules; `reldir lint` proposes them.
- Unobserved means not promised: a member only ever null admits only null, and one sometimes absent but never null is optional, not nullable.
- A change that needs a value the contract does not admit is refused with `CONTRACT_WIDENING_REQUIRED`. Widening is always an explicit, recorded act of a person (`docs/widen-a-contract.md`); reldir never widens on its own.
- A write changes a row file only where its row changed: number spelling, integers beyond 64 bits, timestamp offsets, a string's Unicode form, member order, indentation and line endings stay as they were (`docs/format.md`, "Rows as written").

Every change to inference, the SQL write path, referential actions or row rendering keeps this promise.

## Documentation

- `docs/` follows Diátaxis: one tutorial, how-to guides titled "How to …", reference, and explanation. Each page keeps to one mode and links to the others; `docs/index.md` lists them by mode.
- The docs describe reldir as it is now. They carry no history, no "previously" or "no longer", and no roadmap.
- `docs/install.md` names only what installs today. Check GitHub Releases, crates.io, PyPI and npm before changing it.

## Work tracking

- Work is tracked on the GitHub project board jowharshamshiri/4 (named in `.wsb/project.json`) as draft items, never as issues in this public repository. A pull request never says `Closes #N` for board work; a pull request may itself sit on the board.
- Every card sets Status (Postponed, Backlog, Ready, In progress, Suspended, In review, Done), Priority (P0, P1, P2), Size (XS, S, M, L, XL) and Area (Engine, Writes, Perf, Protocol, MCP, Git, Libraries, Verification, Versioning, Release, Updater, Install, Windows, CI, Docs, wsb, Adopters). Titles carry no area prefix.
- A card's body is a paragraph, then `Done when: …`. News goes on top as `Update <date>: …`.
