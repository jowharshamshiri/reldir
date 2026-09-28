---
title: MCP server
---

# MCP server

`reldir mcp` serves the database to AI agents over the
[Model Context Protocol](https://modelcontextprotocol.io), on stdin and stdout.
Every tool is a reldir command, run exactly as the CLI runs it, and answers with
the same `command_result` envelope `--format json` prints. Changes go through
the same validated transactions: an agent can no more leave the files invalid
than a person can.

## Connecting

Any MCP client that launches a stdio server:

```json
{
  "mcpServers": {
    "reldir": { "command": "reldir", "args": ["--db", "/path/to/data", "mcp"] }
  }
}
```

Global flags given before `mcp` apply to every call: `--readonly` makes the
whole session read-only, and the resource limits bound every query.

## Tools

| Tool | Does | Changes data |
|---|---|---|
| `status` | validity, unrecorded changes, the revision | no |
| `tables` | the tables, row counts, keys | no |
| `describe` | a table's columns, key, references in and out | no |
| `schema_show` | a table's schema document | no |
| `query` | a read-only SQL statement; `allow_invalid` answers from an invalid database | no |
| `mutate` | an `INSERT`, `UPDATE` or `DELETE` | yes |
| `insert_row`, `update_row`, `delete_row` | one row, by key | yes |
| `check` | every violation, located | no |
| `lint` | how the schemas could be stronger | no |
| `doctor_plan` | every repair, least destructive first | no |
| `apply_fixes` | doctor's default fixes, or the one named by `only` | yes |
| `diff` | what changed since the last revision, or between two | no |
| `log` | the revisions | no |

Tools that change data default to `"dry_run": true`: the change is planned,
fully validated and described -- every file, every referential action -- and
nothing is written. Pass `"dry_run": false` to commit it. Anything that removes
data -- `delete_row`, a `DELETE`, a fix that drops a reference or a row --
also needs `"confirm": true`, and otherwise stops with `DECISION_REQUIRED`.

A tool's result is the envelope, as structured content: `ok`, `exit`,
`summary`, `records`, `diagnostics`, `events`, and on failure `error`. A
failed command is a tool error the agent can read, never a protocol error.

## Resources

| URI | |
|---|---|
| `reldir://dialect` | the schema dialect's meta-schema |
| `reldir://schema/<table>` | each table's schema document |
| `reldir://docs/<page>` | these pages: `concepts`, `schemas`, `sql`, `validation`, `errors`, `cli` |
