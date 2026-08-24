---
status: accepted
---

# Keep MPRIS renderer-local for the first capability slice

The first MPRIS implementation stays inside the renderer generation. The shared
capability authority still checks generation IDs and revisions, so stale
commands fail during reload. Move the backend owner to the supervisor only when
reload continuity, public ownership, or overlapping lifetime becomes a real
caller requirement.

## Considered options

- Start with a durable supervisor owner. Rejected because the first slice has no
  continuity or public ownership requirement.
- Let each generation bypass the capability authority. Rejected because stale
  commands would have no common rejection path.

## Consequences

- The first capability slice has one generation-scoped backend owner.
- Reload may reconnect to MPRIS.
- Moving ownership later remains an internal placement change, not a Lua
  interface change.
