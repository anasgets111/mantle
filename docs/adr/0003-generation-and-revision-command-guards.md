---
status: accepted
---

# Guard capability commands with generation and revision

Every state-dependent capability command carries the sender's generation ID and
the expected snapshot revision. The capability authority rejects stale
generation IDs and revisions before the backend adapter validates command
meaning. Commands whose meaning is independent of snapshot state may omit the
revision precondition.

## Consequences

- A dying generation cannot mutate capability state after handoff.
- A command cannot apply to a snapshot the sender never observed.
- Each capability owner still validates its own command semantics.
