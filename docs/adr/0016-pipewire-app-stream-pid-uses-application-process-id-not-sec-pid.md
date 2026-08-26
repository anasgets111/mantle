# Per-app audio stream PID uses `application.process.id`, not `sec.pid` or `node.client-id`

`build-steps.md` Phase 6 says to map a stream node to its owning process via `sec.pid` or `node.client-id`. Neither name is a real PipeWire property key. Checked `pipewire-rs` 0.10.1's own `keys` module (`src/keys.rs`) against `/usr/include/pipewire-0.3/pipewire/keys.h` on a machine with `pipewire`/`pipewire-pulse` actually running, not from memory:

* `PW_KEY_SEC_PID` is `"pipewire.sec.pid"`, not `"sec.pid"`.
* There is no `PW_KEY_NODE_CLIENT_ID`. The real key linking a node to its owning client object is `PW_KEY_CLIENT_ID`, `"client.id"`.

Checking the real key names wasn't enough, though -- `pw-dump` output for actual playback streams shows `pipewire.sec.pid` is the wrong property for this job even once correctly spelled. It lives on the `Client` object, not the stream `Node`, and it's set by the *protocol* to the pid of whatever process opened the connection. For any client routed through `pipewire-pulse` (the PulseAudio compatibility shim, which is how most consumer apps -- browsers, `speech-dispatcher`, etc. -- reach PipeWire), every one of those apps' `Client` objects report `pipewire.sec.pid` as `pipewire-pulse`'s own pid, not the application's:

```
$ pw-dump | jq of Client objects 65, 71, 97 (three unrelated apps' streams):
  client 65 (speech-dispatcher-generic): pipewire.sec.pid = 1975
  client 71 (speech-dispatcher-dummy):   pipewire.sec.pid = 1975
  client 97 (Zen browser):               pipewire.sec.pid = 1975
$ ps -p 1975 -o comm=
pipewire-pulse
```

Following `client.id` from the stream node to the `Client` object and reading `pipewire.sec.pid` from there would have collapsed every PulseAudio-API app's stream onto the same pid -- useless for a per-app mixer.

The property that actually carries the right pid is `application.process.id` (`PW_KEY_APP_PROCESS_ID`, `"application.process.id"`), set directly on the stream *node's* own properties by the client library (PulseAudio-compat clients set it same as native ones). Verified against the same `pw-dump` output and `/proc/{pid}/comm`:

```
node 76 (Zen browser stream): application.process.id = 1538319, application.name = "Zen"
$ cat /proc/1538319/comm
zen-bin
```

matches the real running process in every case checked. `media.class == "Stream/Output/Audio"` (`PW_KEY_MEDIA_CLASS`, `"media.class"`) is exactly as `build-steps.md` names it -- no correction needed there.

A second, related finding from running against the live daemon rather than trusting the property names alone: `application.process.id` isn't present on a `pipewire-pulse`-routed stream's *first* `global` event either. `pipewire-pulse` creates the node, PipeWire advertises it, and only moments later does `pipewire-pulse` push `application.process.id`/`application.name` onto the node's properties, which arrives as the node's own `info` event with the `PROPS` change bit set -- a real instance of "node-properties-changed" as build-steps.md's own diagram names it, not just theoretical. Filtering on the full parse (`media.class` and pid both) at `global` time missed every real stream in a live run; filtering on `media.class` alone at `global` time, then always binding the node and running the full parse inside its `info` callback (which fires once immediately on bind and again on every later property push) picks them all up.

Decision: `supervisor/src/audio/mixer.rs`'s `on_global` checks `media.class` off the registry `global` event's already-known properties, then binds the node unconditionally on a match. `parse_stream_props` (the full `media.class` + `application.process.id` parse) runs inside the bound node's `info` callback instead, which is what `main.rs`-facing snapshots are actually built from. No `client.id` lookup or separate `Client` bind is needed either way. `resolve_process_name` then reads `/proc/{pid}/comm` for the resolved pid, per the spec's own suggested resolution method.

This does not touch `pipewire.sec.pid`'s legitimate uses elsewhere (e.g. permission/security decisions the protocol layer already makes) -- it's the wrong property specifically for "which application owns this audio stream," which is what this phase asks for.
