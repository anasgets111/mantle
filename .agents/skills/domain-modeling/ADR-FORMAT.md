# ADR Format

ADRs live in `docs/adr/` and use sequential numbering: `0001-slug.md`, `0002-slug.md`, etc.

Create the `docs/adr/` directory lazily: only when the first ADR is needed.

## Template

```md
# {Short title of the decision}

{1-3 sentences: what's the context, what did we decide, and why.}
```

That's it. An ADR can be a single paragraph. The value is in recording *that* a decision was made and *why*, not in filling out sections.

## Optional sections

Only include these when they add genuine value. Most ADRs won't need them.

- **Status** frontmatter (`proposed | accepted | deprecated | superseded by ADR-NNNN`): useful when decisions are revisited
- **Considered Options**: only when the rejected alternatives are worth remembering
- **Consequences**: only when non-obvious downstream effects need to be called out

## Numbering

Scan `docs/adr/` for the highest existing number and increment by one.

## When to offer an ADR

All three of these must be true:

1. **Hard to reverse**: the cost of changing your mind later is meaningful
2. **Surprising without context**: a future reader will look at the code and wonder "why on earth did they do it this way?"
3. **The result of a real trade-off**: there were genuine alternatives and you picked one for specific reasons

If a decision is easy to reverse, skip it: you'll just reverse it. If it's not surprising, nobody will wonder why. If there was no real alternative, there's nothing to record beyond "we did the obvious thing."

### What qualifies

- **Architectural shape.** "Renderer generations run in separate processes." "One reload transaction owns activation, presentation evidence, freeze ordering, rollback, and reaping."
- **Integration patterns between owners.** "The loader and watcher consume one dependency snapshot." "The supervisor and renderer use a private control protocol."
- **Technology choices that carry lock-in.** SCTK, femtovg on EGL, SHM as the headless test adapter, or vendored Lua 5.4. Record only choices that would be costly to replace.
- **Boundary and scope decisions.** "The capability authority owns revisions and stale-command rejection; backend adapters validate command meaning." The explicit no-s are as valuable as the yes-s.
- **Deliberate deviations from the obvious path.** "The first MPRIS adapter stays renderer-local instead of moving to the supervisor." Record choices that a future maintainer might otherwise "fix."
- **Constraints not visible in the code.** "Every targeted output must provide presentation evidence before authority changes." Record protocol and cleanup constraints that callers cannot infer.
- **Rejected alternatives when the rejection is non-obvious.** If a durable MPRIS owner was considered and rejected for the first slice, record why. Otherwise it will be proposed again later.
