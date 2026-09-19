# Ledger — a worked example, and an attack on it

A small double-entry-ish ledger built on the reldir Python driver, plus a stress
suite that tries to break it.

```sh
python3 setup.py /tmp/ledger $(which reldir)   # build the database
python3 -u stress.py                            # attack it
```

## The application

`ledger.py` is the part an application author would write. Three tables with
real constraints, so the stress suite has something to violate:

| Table | What it carries | What constrains it |
|---|---|---|
| `accounts` | balance, email, status | `CHECK balance >= 0`, unique email, id pattern, status enum |
| `orders` | amount, note | FK to `accounts` with `restrict`, generated uuid and timestamp |
| `audit` | every balance change | append-only, generated uuid and timestamp |

Two details are deliberate and worth reading before copying this code.

**Every balance change is a compare-and-set.** reldir cannot detect a stale
read, because each `UPDATE` is complete and valid on its own. So `debit` carries
the balance it read into the predicate:

```python
tx.execute(
    "UPDATE accounts SET balance = ? WHERE id = ? AND balance = ?",
    [row["balance"] - amount, account, row["balance"]],
    retry_on_conflict=False,
)
if result.changed == 0:
    raise reldir.Stale(...)     # the block re-reads and tries again
```

Without `AND balance = ?`, two concurrent debits both read the same figure, both
succeed, and the account is short by one of them. The stress suite checks for
exactly that.

**`transfer` and `place_order` are not atomic.** reldir commits one statement at
a time, so a transfer is a debit followed by a credit, and a reader between them
sees the money in neither account. The suite therefore asserts conservation *at
rest*, after every thread has joined — not at arbitrary instants, which is a
mistake that makes a correct system look broken.

An application that needs the pair to be atomic has to model it as one row.

## The stress suite

`stress.py` runs eight scenarios, each asserting an invariant that has a real
way to come out wrong:

| # | Scenario | What would be a defect |
|---|---|---|
| 1 | concurrent debits on one account | balance disagrees with granted debits; negative balance; audit disagrees |
| 2 | concurrent transfers | total money changes |
| 3 | `SIGKILL` mid-commit, then recover | recovery fails, or leaves an invalid database |
| 4 | external corruption of a row | reldir absorbs bad JSON or a missing field instead of reporting it |
| 5 | injection through parameters | a hostile value executes as SQL instead of landing as data |
| 6 | schema and metadata tampering | an unknown format version or loosened pin is accepted |
| 7 | readers during sustained writes | a read is refused for contention, or sees an impossible total |
| 8 | resource limits | a limit truncates silently instead of failing |

Findings stream to the terminal **and** to `stress-findings.txt` as they happen.
That is not incidental: the first run of this suite was interrupted after twelve
minutes and reported nothing at all, because Python buffers `print()` when
stdout is not a terminal. A long-running attack that loses its results when
stopped is worthless, so every line is flushed and journalled immediately.

## Reading the results honestly

Two traps, both of which caught me while writing this:

**Do not measure a live database.** Computing a total while transfers are in
flight shows money apparently missing, because a transfer is two statements.
It reconciles the moment the writers stop. Invariants belong after `join()`.

**A swallowed exception hides the defect it should reveal.** Scenario 2 catches
`reldir.Stale` and continues; if a credit genuinely exhausted its retry budget,
conservation would break and the report would show the mismatch without saying
why. Worth remembering when a scenario reports a number you cannot explain.
