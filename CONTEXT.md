# Oblisk shell

Shared language for renderer generations, reloads, retained scenes, and capability state.

## Reloads

**Generation**:
A renderer process and its Lua state with one generation ID.
_Avoid_: instance, worker

**Candidate**:
A generation being prepared but not yet authoritative.
_Avoid_: active generation, staged shell

**Authoritative generation** (per output):
The generation that receives input, reserves exclusive space, and owns capability routing for one output. Ownership transfers per output during a swap; a generation is fully authoritative once it owns every output it targets.
_Avoid_: active generation, current process

**Presentation evidence**:
Proof that a targeted output produced its first frame or presentation feedback after configuration.
_Avoid_: readiness, activation ACK

**Topology change**:
A config edit that adds, removes, or changes the layer, anchor, or monitor target of a top-level `surface` node. Triggers a generation swap.
_Avoid_: structural change, breaking change

**Value change**:
A config edit that is not a topology change. Triggers an in-place reload of the same generation.
_Avoid_: minor change, hot patch

**Generation swap**:
The full candidate-spawn, presentation-evidence, promote-and-reap flow. Reserved for topology changes.
_Avoid_: hot-reload (ambiguous: covers both swap and in-place reload)

**In-place reload**:
Resetting the Lua VM and re-running the config inside the current generation, without spawning a candidate or rebinding Wayland/EGL. Used for value changes.
_Avoid_: hot-reload, live patch

## Surfaces

**Wallpaper surface**:
The third static surface (`Background` layer, non-exclusive, one per monitor), distinct from `main_bar` and `overlay_canvas`. Owns wallpaper texture rendering.
_Avoid_: background layer (protocol term, not the Oblisk surface)

## Ownership

**Lock authority**:
The Supervisor-held `ext_session_lock_v1` handle and its minimal `wl_shm` fallback surface. Outlives the Renderer; distinct from the Lua-authored lock screen widget, which is presentation only and can die with the Renderer.
_Avoid_: lock screen (ambiguous: covers both the authority and the Lua widget)

## Capabilities

**Capability**:
A named IPC-addressable module owning one slice of state and its write actions (e.g. `audio`, `network`).
_Avoid_: module, service, backend

**Revision**:
A capability's state-version counter. A stale revision fails the write.
_Avoid_: version, sequence number

**Secure submit**:
A `textfield` property naming the capability/action that receives a masked field's native input buffer directly, bypassing Lua. Without it, a masked field's value is unreadable from Lua entirely.
_Avoid_: secure handle, password callback
