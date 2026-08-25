# Oblisk shell

Shared language for renderer generations, reloads, retained scenes, and
capability state.

## Reloads

**Generation**:
A renderer process and its Lua state with one generation ID.
_Avoid_: instance, worker

**Candidate**:
A generation being prepared but not yet authoritative.
_Avoid_: active generation, staged shell

**Authoritative generation**:
The generation that receives input, reserves exclusive space, and owns
capability routing.
_Avoid_: active generation, current process

**Presentation evidence**:
Proof that a targeted output has produced its first frame or presentation
feedback after configuration.
_Avoid_: readiness, activation ACK

## Scene and configuration

**Builder**:
A Lua function that returns scene descriptors.
_Avoid_: component, widget factory

**Retained scene**:
A scene whose nodes keep identity while the shell refreshes.
_Avoid_: rebuilt tree, disposable scene

**Dependency snapshot**:
The bounded set of configuration files and watch roots captured for a
generation.
_Avoid_: watch list, file scan

## Capabilities

**Capability**:
A named system capability with bounded state and shell commands.
_Avoid_: service, platform object

**Dormant**:
A capability no generation has claimed yet: it owns no thread, D-Bus name, or
system resource until a `require()` triggers registration.
_Avoid_: unavailable, inactive, disabled

**Unavailable**:
A claimed capability whose backend cannot currently serve it (missing
hardware, compositor feature, or dependency). Publishes unavailable state,
never nil.
_Avoid_: dormant, disabled, off

**Revision**:
A version attached to a capability snapshot so commands using stale state can
be rejected.
_Avoid_: timestamp, refresh count
