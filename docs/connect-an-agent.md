---
title: How to give an AI agent access to a database
---

# How to give an AI agent access to a database

This guide connects an AI agent to a reldir database through the Model Context
Protocol, so the agent can query, check and change it under the same
validation as the command line. It assumes `reldir` is on the `PATH` of the
program that runs the agent. The tools, resources and prompts the server
offers are listed in the [MCP reference]({{ site.baseurl }}/mcp).

## Add the server to Claude Code

From the project that holds the database, add the server and name the
database's folder:

```sh
claude mcp add reldir -- reldir --db ./data mcp
```

## Add the server to another client

Any client that launches MCP servers on stdin and stdout takes the same
command. In a client configured by JSON, give the database's absolute path,
since the client may start the server from any directory:

```json
{
  "mcpServers": {
    "reldir": { "command": "reldir", "args": ["--db", "/path/to/data", "mcp"] }
  }
}
```

## Let the agent read but never write

If the agent should only look -- to answer questions, or to report faults --
put `--readonly` before `mcp`:

```sh
claude mcp add reldir-readonly -- reldir --db ./data --readonly mcp
```

The session then writes nothing at all: no rows, no history, no derived state.
Every tool that changes the database answers `READ_ONLY`.

## Bound what a query may cost

Flags before `mcp` apply to every call of the session. To stop a runaway query
or an answer too large for the agent's context, give limits:

```sh
claude mcp add reldir -- reldir --db ./data --timeout 5 --max-result-rows 5000 mcp
```

`query` also answers a page at a time, 200 rows unless the agent asks for
more. The other limits are listed in [configuration]({{ site.baseurl }}/configuration).

## Approve the changes the agent makes

The agent cannot change the database in one step. Each change tool first
returns a plan -- every file it would write or delete, every referential
action -- and commits only when called again with that plan's `plan_id`, and
only if the database has not changed since.

When a plan removes data -- deletes a row, or runs a referential action -- it
also needs your confirmation:

- if your client supports elicitation, the server asks you directly, naming
  what the plan removes; decline, and nothing is written;
- otherwise the agent must pass `confirm: true`. Tell the agent, in its
  instructions or in the conversation, to ask you before it does.

Every change is a revision in history, so a change you regret can be undone;
see [How to undo a change]({{ site.baseurl }}/undo-a-change).

## Give the agent a starting point

The server offers prompts that set the agent to a task. In a client that
lists prompts, choose one:

- `explore-database`, to learn the tables, how they relate and what is wrong,
  changing nothing;
- `design-schema`, to design or change one table, showing each plan;
- `repair-database`, to find every fault and repair it, confirming removals
  with you.

## Keep a large folder fast for the agent

Every call reads the folder as it is at that moment. On a large folder, run
`reldir serve` beside the agent, so each call examines only what changed; see
[How to keep a large folder fast]({{ site.baseurl }}/keep-a-large-folder-fast).
