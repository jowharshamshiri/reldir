---
title: How to check a database in continuous integration
---

# How to check a database in continuous integration

This guide makes every push and pull request check the database in a
repository, fail the job when the database is invalid, and show each fault on
the line it is on. It assumes the database is versioned in the repository with
its `.db/.gitignore` defaults (see [what git versions]({{ site.baseurl }}/git#what-git-versions)).

## On GitHub Actions

1. Give the workflow permission to upload code scanning results, and run the
   repository's check action on the database's folder. Here the database is
   `data/`:

   ```yaml
   name: data
   on: [push, pull_request]

   permissions:
     contents: read
     security-events: write

   jobs:
     check:
       runs-on: ubuntu-latest
       steps:
         - uses: actions/checkout@v5
         - uses: jowharshamshiri/reldir/.github/actions/check@main
           with:
             path: data
   ```

2. Commit the workflow as `.github/workflows/data.yml`.

The job fails when the database is invalid. Each fault appears as a code
scanning alert on the row file and line it is on, and the job log shows the
same check as a table.

- If lint findings and warnings should fail the job too, add `strict: true`.
- If the repository cannot use code scanning, add `upload: false`, and drop
  `security-events: write`.
- To pin the reldir the job runs, name the action by a tag or a commit
  instead of `@main`: the action builds reldir from its own source at that
  ref, which adds a couple of minutes to the job.

The [action's inputs]({{ site.baseurl }}/git#the-check-action) are listed in
the reference.

## On any other CI system

1. Install reldir in the job; see [How to install reldir]({{ site.baseurl }}/install).
2. Run the check read-only, writing a SARIF log:

   ```sh
   reldir --db data --readonly --format sarif check > reldir.sarif
   ```

   `--readonly` writes nothing at all -- no history, no derived state -- so the
   job leaves the checkout as it found it, and a folder without `.db/` is
   checked as it would be adopted.
3. Let the command's exit status decide the job, and hand `reldir.sarif` to
   any tool that reads SARIF 2.1.0. Each rule in the log links to its code in
   the [error reference]({{ site.baseurl }}/errors).

To print the faults in the job log as well, run the check a second time with
`--format table`.

For lint findings and warnings to fail the job, add `--strict` to `check`.

## Act on the exit status

| Status | Means | Do |
|---|---|---|
| `0` | valid | nothing |
| `2` | invalid | fix the faults the log names; see [How to repair an invalid database]({{ site.baseurl }}/repair-a-database) |
| `7` | valid, with findings under `--strict` | act on the findings, or drop `--strict` |
| `6` | the format, history or metadata in `.db/` cannot be trusted | see [How to recover]({{ site.baseurl }}/recover) |
| `1` | the path, a flag or a setting is wrong | correct the job |

Every status is listed in the [error reference]({{ site.baseurl }}/errors#exit-statuses).

## Check after every merge locally

To have each clone check the database after every merge and checkout, as CI
does after every push, see
[How to merge and diff row files with git]({{ site.baseurl }}/merge-with-git).
