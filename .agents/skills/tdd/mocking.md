# When to Mock

Mock at **system seams** only:

- Wayland compositor protocol and other external system APIs
- Capability backends such as MPRIS or D-Bus
- Time and randomness
- Filesystem access at the dependency-snapshot seam
- Renderer process launch and OS signals

Don't mock:

- Your own modules
- Internal collaborators
- Anything you control

## Designing for Mockability

At system seams, design interfaces that are easy to mock:

**1. Pass adapters into the module**

Pass an external adapter in rather than creating it internally:

```rust
// Easy to mock
fn apply_mpris_command(command: Command, backend: &dyn MprisBackend) -> Result<()> {
    backend.apply(command)
}

// Hard to mock
fn apply_mpris_command(command: Command) -> Result<()> {
    let backend = MprisBackend::connect()?;
    backend.apply(command)
}
```

**2. Prefer specific adapter operations over generic dispatch**

Expose the backend operations needed by a capability instead of one generic
dispatcher with conditional logic:

```rust
// GOOD: Each operation has one bounded command shape
trait MprisBackend {
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn next(&mut self) -> Result<()>;
}

// BAD: The mock must reimplement command routing
trait CapabilityBackend {
    fn dispatch(&mut self, command: CapabilityCommand) -> Result<()>;
}
```

Specific operations keep mock setup small and make the capability command
under test visible.
