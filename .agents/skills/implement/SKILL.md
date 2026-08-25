---
name: implement
description: "Implement an Oblisk slice from the plan or build steps."
disable-model-invocation: true
---

Implement the requested Oblisk slice against `docs/build-steps.md`, the
relevant ADRs in `docs/adr/`, and the user's request. Use the canonical terms
in `CONTEXT.md`.

Use `/tdd` where possible, at pre-agreed module seams. Keep Rust ownership in
the engine and supervisor, and keep Lua focused on generation configuration and
scene composition.

Run `cargo check --workspace` regularly. Run
`cargo clippy --workspace --all-targets --all-features -- -D warnings` before
considering any slice done, not just at the very end. Run focused `cargo test`
commands for the touched crate or test filter, then run `cargo test --workspace`
once at the end. Run the headless Wayland or real-session check required by the
build step when the change crosses a protocol or capability seam.

Once done, use `/oblisk-review` to review the work against the requested slice.

Do not commit unless the user asks for a commit.
