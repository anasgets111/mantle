# CONTEXT.md Format

## Structure

```md
# Mantle engine

Engine vocabulary. Terms a config author meets belong in `docs/glossary.md`; this file holds engine-internal terms and engine notes on glossary ones.

## Processes and ownership

| Term | Meaning |
| :--- | :--- |
| **Authoritative generation** | The generation the Supervisor sends to and accepts frames from. A replacement becomes authoritative when spawned and is hydrated when it connects. |
| **Capability** | A Supervisor module. Not "service" or "backend". |

## Reloads

| Term | Meaning |
| :--- | :--- |
| **Watcher** | The Supervisor's inotify observer of the config tree (`.lua`, `.frag`). A debounced save with changed content sends `Reevaluate` to the authoritative generation. |
| **Loader** | The Renderer's evaluation of `shell.lua` into a node tree and surface specs. |
```

## Rules

- **Be opinionated.** When multiple words exist for the same concept, pick the canonical one and list rejected synonyms inline at the end of the row: `Not "service" or "backend".`
- **Keep definitions tight.** One or two sentences max. Define what it IS, not what it does.
- **Only include terms specific to this project's context.** General programming concepts (timeouts, error types, utility patterns) don't belong even if the project uses them extensively. Before adding a term, ask: is this a concept unique to this context, or a general programming concept? Only the former belongs.
- **Group terms under subheadings** when natural clusters emerge.

## Scope

Mantle has one engine context in root `CONTEXT.md`. Terms a config author meets belong in `docs/glossary.md`. Do not split `CONTEXT.md` into renderer and capability contexts unless their terms or ownership rules genuinely diverge.
