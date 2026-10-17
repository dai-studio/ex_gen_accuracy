# ex_gen_accuracy

An experiment to measure how the **form of the input context** affects code
generation. The same project, **Moxy** (a local AI gateway server written in
Rust with Pingora, a SvelteKit UI and a redb key-value store), is built three
times by a fresh Claude Code session. Each build gets the requirements in a
different format.

## Inputs

| Variant     | Input file           | Size      | What it is |
|-------------|----------------------|-----------|------------|
| `prompt_md` | `inputs/prompt.md`   | 1 paragraph | A short natural-language prompt. No stack, schema, routes or decisions. |
| `spec_md`   | `inputs/spec.md`     | ~200 lines | A full prose specification: dependencies, config schema, routes, middleware, data model, decisions, assumptions. |
| `spec_dai`  | `inputs/spec.dai`    | ~550 lines | The same specification as a BPMN 2.0 diagram with `dai:` extension attributes (typed nodes, properties, flows, decisions, assumptions). |

`spec.md` and `spec.dai` hold the same information. The difference is the
structure: prose versus a typed graph. `prompt.md` is the low-context baseline.

## Layout

```
inputs/                 originals (never touched by a run)
outputs/
  prompt_md/prompt.md   working dir for the prompt run
  spec_md/spec.md       working dir for the prose-spec run
  spec_dai/spec.dai     working dir for the .dai run
run.sh                  starts one clean session for one variant
stats.py                writes outputs/<variant>/stats.md (tokens and cost)
```

Each output folder starts out holding only its own copy of the input. The
session writes all of its code there. When the session ends, two files are
added:

- `.session_id`: the session's UUID, which `run.sh` pins with `--session-id`.
- `stats.md`: the run's token usage and cost.

## Running

```sh
./run.sh prompt_md
./run.sh spec_md
./run.sh spec_dai
```

Each call starts an **interactive** Claude Code session in
`outputs/<variant>/`, using the same kickoff prompt every time:

> Build the project described in `./<input file>`. Implement it completely in
> this directory, make it build, and add tests where they make sense. When you
> finish, write NOTES.md summarising what you built, any decisions you had to
> make yourself, and anything you left unimplemented.

Run the variants one after another, in separate terminals, or in parallel.
They don't share any state.

### Token and cost stats

When you exit a session (`/exit` or Ctrl+D), `run.sh` runs `stats.py`. That
script reads the session's transcript from `~/.claude/projects/*/<session-id>.jsonl`
(or `$CLAUDE_CONFIG_DIR` if you set it) and writes `outputs/<variant>/stats.md`
with the following:

- Session details: model, effort, Claude Code version, start and end times,
  wall-clock, API and tool durations, message and tool-call counts, and lines
  added and removed.
- A table for each model with input, output, thinking, cache-read and
  cache-write tokens, web searches and cost. Subagent and background-model
  usage is included.

The totals come from the transcript's last `cost-state` record, which holds
the same numbers `/cost` shows. If that record is missing, the script sums
tokens from the assistant messages and reports the cost as `n/a`.

If you resume a session with `claude --resume` and keep working, refresh its
stats afterwards:

```sh
./stats.py spec_dai
```

### Keeping runs comparable

Use the same settings for all three runs. The defaults are below, and you can
override any of them with environment variables:

| Variable          | Default       |
|-------------------|---------------|
| `MODEL`           | `opus`        |
| `EFFORT`          | `high`        |
| `PERMISSION_MODE` | `acceptEdits` |

```sh
MODEL=sonnet EFFORT=medium ./run.sh spec_dai
```

### What "clean" means

`run.sh` launches `claude` with these flags:

- `--safe-mode`: turns off CLAUDE.md files, auto-memory, skills, plugins,
  hooks, MCP servers, custom agents and output styles. Your login and the
  built-in tools still work.
- `--strict-mcp-config` and `--no-chrome`: no MCP servers and no browser
  tools.
- `--settings` with deny rules: the session can't `Read` `inputs/`, and it
  can't `Read` or `Edit` the other two variant folders.
- `--append-system-prompt`: tells the session that `./<input file>` is its
  only source of requirements and that it must stay inside its working
  directory.
- `--name moxy-<variant>`: makes the session easy to find in `/resume`.

**Limitation:** the deny rules cover the file tools only. A shell command such
as `cat ../../inputs/spec.md` would get around them. The system-prompt
instruction is what discourages that. For strict isolation, copy
`outputs/<variant>/` to a directory outside this repo and run the session
there. Web access is left on, because the build may need to look up crate and
npm versions.

## Evaluating results

When the runs finish, compare the three folders on criteria such as these:

- **Builds and runs:** `cargo build`, `cargo test`, and the UI build.
- **Spec conformance:** go through the spec's requirements, for example
  reserved routes, route-matching precedence, the JSON error shape,
  middleware order, status codes (401/403/413/422/429/502/504), KV key
  formats, cookie flags and API key format, and record which ones each build
  implemented.
- **Self-made decisions:** check each `NOTES.md`. The `prompt_md` run has to
  invent nearly everything. The spec runs should invent very little.
- **Scope and size:** count files and lines of code, and note any features
  that were left out or stubbed.
- **Effort:** tokens, cost and wall-clock time for each session, taken from
  each folder's `stats.md`.
