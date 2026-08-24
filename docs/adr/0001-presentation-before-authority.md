---
status: accepted
---

# Require presentation evidence before changing authority

A candidate does not become authoritative on `READY` or an activation ACK. Every
targeted output must complete its configure step and provide first-frame or
presentation evidence before the supervisor freezes the old generation and
changes authority. This prevents a multi-output handoff from switching only the
outputs that happened to respond first.

## Considered options

- Change authority on activation ACK. Rejected because ACK proves permission to
  commit, not presentation.
- Wait for one output only. Rejected because another targeted output may remain
  blank or stalled.

## Consequences

- Untargeted outputs do not delay activation.
- A stalled targeted output keeps the old authoritative generation live.
- The reload transaction needs per-output evidence and a wall-clock deadline.
