# Oblisk Advanced Scaffold & Scoped Engineering Roadmap (v2)
## Multi-Crate Rust Cargo Workspace Scaffolding and Research Playbook

This document specifies the exact, step-by-step scaffolding architecture and compiler playbooks to guide a coding agent from zero to a successfully compiled binary. It includes targeted **Research Milestones** with concrete technical criteria and direct links to active open-source GitHub repositories to solve complex platform boundaries.

---

## 1. Architectural Workspace Topology

The Oblisk codebase is laid out as a multi-crate cargo workspace, enforcing a hard boundary between the privileged, durable platform daemon (`supervisor`) and the unprivileged, hot-reloadable graphics UI renderer (`renderer`).

```text
oblisk-workspace/
├── Cargo.toml                      # Workspace meta-configuration
├── Cargo.lock
├── shared/                         # Serialization definitions & IPC protocols
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs                  # Guarded envelopes, JSON-RPC, snapshots
├── supervisor/                     # Durable background system daemon (zbus/pipewire)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # Unix socket server, core event loop, and signal router
│       ├── dbus/                   # NetworkManager, BlueZ, MPRIS, and Polkit interfaces
│       ├── hardware/               # Udev, Netlink, and Sysfs polling threads
│       ├── process/                # Safe process group spawning & PGID reaping
│       └── reload/                 # Presentation-Before-Authority orchestrator
└── renderer/                       # Ephemeral UI Renderer process (Wayland/GLES3/Lua)
    ├── Cargo.toml
    └── src/
        ├── main.rs                 # CLI entry point, MLua VM initializer, frame thread
        ├── wayland/                # SCTK client wrappers & static surface mapping
        ├── layout/                 # One-pass constraint solver & subpixel snapping
        ├── render/                 # FemtoVG path drawing & GLES3 transition shaders
        └── lua/                    # MLua userdata proxy signal bindings
```

---

## 2. Scaffolding Playbook: Phase-by-Phase Compiler Milestones

### Phase 1: Workspace Scaffolding & Cargo Dependency Tree

Establish the compilation parameters in the workspace root. Use the latest stable Rust edition and toolchain available at implementation time, not a pinned historical one; edition 2024 in particular tightens `unsafe` block requirements in ways that directly serve this project's memory-safety goals in the Wayland/EGL FFI code. Pair that with highly aggressive production profile configurations to eliminate GC-adjacent stutters in hot paths (there's no GC to stutter, but the same profile settings still remove allocator and codegen overhead).

Every crate version below is a placeholder (`"latest"`), not a real pin. Run `cargo add <crate> --features ...` or check crates.io directly at implementation time to resolve actual versions. This matters more for some crates than others: `zbus` in particular has moved through multiple major versions (3 → 4 → 5) with real breaking API changes since this spec was drafted, so check its migration notes specifically rather than assuming the API described elsewhere in these docs still matches the current major version.

#### 1. Root `Cargo.toml`
```toml
[workspace]
members = ["shared", "supervisor", "renderer"]
resolver = "2"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

#### 2. `shared/Cargo.toml`
```toml
[package]
name = "shared"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "latest", features = ["derive"] }
serde_json = "latest"
thiserror = "latest"
```

#### 3. `supervisor/Cargo.toml`
```toml
[package]
name = "supervisor"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "latest", features = ["full"] }
zbus = { version = "latest", features = ["tokio"] }
tokio-stream = "latest"
futures-util = "latest"
nix = { version = "latest", features = ["process", "signal"] }
regex = "latest"
pipewire = "latest"
udev = "latest"
inotify = "latest"
wayland-client = "latest"
wayland-protocols = { version = "latest", features = ["client"] }
smithay-client-toolkit = "latest"
```

`pipewire`, `udev`, and `inotify` were missing from this list despite being required by the prose elsewhere in this doc and in `oblisk-supervisor-services-dbus.md`: `pipewire` for § 6's registry stream mixer (Phase 6 below), `udev` for § 1.1's battery netlink monitor, `inotify` for § 1.2's backlight watch and the config-directory watch driving reload (ADR-0001).

The Supervisor also gets its own Wayland connection now (ADR-0010): `ext_idle_notifier_v1` (§7) and lock-screen authority both need to survive a Renderer crash or reload, which means the process holding them needs to be the Supervisor, not the Renderer. `smithay-client-toolkit`'s `session_lock` module wraps `ext_session_lock_v1`; idle-notify has no SCTK wrapper and is hand-dispatched against raw `wayland-protocols`, the same shape as ADR-0009's `TextInputService` on the Renderer side.

#### 4. `renderer/Cargo.toml`
```toml
[package]
name = "renderer"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "latest", features = ["rt", "net", "macros"] }
mlua = { version = "latest", features = ["lua54", "vendored"] }
wayland-client = "latest"
wayland-protocols-wlr = { version = "latest", features = ["client"] }
wayland-protocols = { version = "latest", features = ["client", "unstable"] }
smithay-client-toolkit = "latest"
khronos_egl = { version = "latest", features = ["static"] }
gl = "latest"
femtovg = "latest"
cosmic-text = "latest"
```

`wayland-protocols` now also carries the `unstable` feature: `wp-text-input-v3`'s client bindings (`textfield`, ADR-0009) live behind it, and `smithay-client-toolkit` was added per ADR-0008.

---

### Phase 2: Core IPC Marshalling and Serialization Layer

Implement the strict serialization models inside `shared/src/lib.rs`. This forms our binary communication interface contract, mapped exactly to `oblisk-idl-api-specs.md` (§ 1 & § 3).

*   **CommandEnvelope**: Wraps Lua write operations with generational tracking flags.
*   **StateSnapshot**: Emitted by the Supervisor on system changes to instantly hydrate active Lua signals.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub jsonrpc: String,
    pub method: String,
    pub params: CommandParams,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandParams {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub arguments: Vec<serde_json::Value>,
    pub expected_revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub revision: u32,
    pub payload: serde_json::Value,
}
```

---

### Phase 3: Research Milestone — High-Performance Wayland EGL Surface Setup

Before rendering any pixels on screen, the `renderer` must initialize an OpenGL ES (GLES3) rendering context bound to the Wayland layer-shell compositor protocols. This has been a frequent source of thread collisions and memory leaks for coding agents.

```text
               [ Wayland Registry Event Dispatcher ]
                                 │
                     (Bind Core Wayland Objects)
                                 ▼
               [ wl_compositor, wl_subcompositor ]
               [ zwlr_layer_shell_v1             ]
                                 │
                      (Initialize EGL Context)
                                 ▼
               [ khronos_egl::Display / Config ]
               [ khronos_egl::Context (GLES3)  ]
                                 │
                        (Create EGL Surface)
                                 ▼
               [ eglCreateWindowSurface(wl_surface) ]
```

#### Research Task & Goals:
1.  **Registry and layer-shell binding**: use `smithay-client-toolkit`'s `shell::wlr_layer` module for `zwlr_layer_shell_v1`/`zwlr_layer_surface_v1` instead of hand-dispatching the protocol against raw `wayland-client` (ADR-0008). Reserve raw `wayland-protocols` for the one thing SCTK doesn't wrap: `wp-text-input-v3` for `textfield`.
2.  **EGL Context Allocation**: Initialize EGL with `khronos_egl`, following SCTK's own EGL setup as a reference: [SCTK EGL Module](https://github.com/Smithay/client-toolkit/tree/main/src/egl). Find the optimal EGL config supporting 8-bit ARGB color formats (`EGL_SURFACE_TYPE` with `EGL_WINDOW_BIT`, `EGL_RENDERABLE_TYPE` with `EGL_OPENGL_ES3_BIT`).
3.  **Surface Context Binding**: Match the Wayland physical native window (`wl_egl_window`) to the created EGL surface and make the GLES3 rendering context current on the thread. Refer to `noctalia`'s OpenGL ES Renderer initialization logic: [Noctalia Renderer Setup](https://github.com/noctalia-dev/noctalia/tree/main/src/renderer).
4.  **Static Layer Constraints**: Register the three static surfaces returned by your layout (ADR-0007):
    *   `main_bar`: anchored on top, marked exclusive.
    *   `overlay_canvas`: anchored to all four edges, non-exclusive, transparent. On boot, immediately commit an empty input region (`wl_compositor::create_region` with no added coordinates) to allow background applications to receive pointer clicks.
    *   `wallpaper_layer`: `Background` layer, non-exclusive, one per monitor.

---

### Phase 4: Research Milestone — Cosmic-Text and FemtoVG Rendering Engine

Text rendering on a Linux status bar must be shaped, wrapped, and cached with extreme efficiency to sustain a fluid 120Hz display refresh cycle.

```text
[ Lua String / Signal ] ──▶ [ cosmic_text::Buffer ] ──▶ [ Off-Thread Swash Glyphs ]
                                                                   │
                                                           (Atlas Packaging)
                                                                   ▼
[ Physical Frame Draw ] ◀── [ FemtoVG Rasterizer ] ◀── [ Glyphs Texture Atlas ]
```

#### Research Task & Goals:
1.  **Asynchronous Shaping**: Write an asynchronous wrapper around `cosmic-text`'s font shaping database (`cosmic_text::FontSystem`, `cosmic_text::Buffer`). Ensure layout widths and font-family fallbacks are processed off-thread to prevent frame drops when rendering dynamic media titles. Refer to cosmic-text implementations: [Cosmic Text Examples](https://github.com/pop-os/cosmic-text).
2.  **Texture Atlas Management**: Map glyphs to a dynamic, size-bounded GPU texture atlas cache (2048x2048) in FemtoVG. Review iced's text graphics engine pipeline: [Iced Text Renderer](https://github.com/iced-rs/iced/tree/master/graphics) and [Noctalia Text Pipeline](https://github.com/noctalia-dev/noctalia/blob/main/src/renderer/text_renderer.cpp).
3.  **Subpixel Snapping Math**: Implement layout snapping math. Calculate text lines and box borders using fractional coordinates, snap coordinates to physical boundaries before damage rectangles are projected to the viewport, and snap borders strictly to single physical pixels to prevent anti-aliasing blur [oblisk-layout-engine-geometry.md § 5].

---

### Phase 5: Research Milestone — Polkit D-Bus Authorization Agent Handshake

A PolicyKit authentication agent must securely receive D-Bus authorization queries and process password authentication off-thread without exposing sensitive keys to the Lua VM heap.

```text
[ privileged action ] ──▶ [ org.freedesktop.PolicyKit1 ]
                                       │
                         (dbus authentication request)
                                       ▼
                             [ Oblisk Supervisor ]
                                       │
                      (push challenge metadata over IPC)
                                       ▼
                             [ Lua VM Dialog UI ]
                                       │
                      (secure password entry typed in)
                                       ▼
[ pam authorization ] ◀── [ Oblisk Secure Buffer ] (typed password)
```

#### Research Task & Goals:
1.  **Agent Registration**: Map out the exact D-Bus signature for `org.freedesktop.PolicyKit1.Authority.RegisterAgent`. Handle standard interactive challenges, authenticating local users on the active session bus. Refer to standard C++/JS implementations: [LXQt Polkit Agent Core](https://github.com/lxqt/lxqt-policykit/tree/master/src) and [Aylur's GTK Shell Polkit Service](https://github.com/Aylur/ags/tree/main/src/service/polkit.ts).
2.  **The Secure Password Input Boundary**: Ensure the `on_change` and `on_submit` callbacks for your `textfield` primitive never capture plain-text characters in Lua VM space. All typed passwords must flow directly into native, secure Rust buffers that zeroize their memory allocations on drop (`secrecy` crate or raw pointer zeroing), preventing keylogger memory extraction attacks.

---

### Phase 6: Research Milestone — PipeWire Registry Stream Mixer

The supervisor must run a zero-polling registry listener that intercept volume level properties changes on the physical ALSA sinks and maps application-specific audio streams dynamically.

```text
[ pw_registry event ] ──▶ [ Intercept Node Added / Properties Changed ]
                                           │
                         (filter by Stream/Output/Audio)
                                           ▼
                                [ Oblisk Audio Apps ]
                                           │
                         (map Process PID -> App Name)
                                           ▼
                         [ Push updated lists to Lua ]
```

#### Research Task & Goals:
1.  **PipeWire Registry Mapping**: Set up a background event thread using `libpipewire` (or raw pipewire socket dispatching). Avoid high-frequency polling commands. Refer to native Rust examples: [Pipewire-rs Examples](https://github.com/rnumr/pipewire-rs) and [AGS Audio Service](https://github.com/Aylur/ags/tree/main/src/service/audio.ts).
2.  **App Mixer Tracking**: Monitor node added and node properties changed events. Identify stream nodes (type `Stream/Output/Audio`) and dynamically map the node's process identifier (`sec.pid` or `node.client-id`) to resolve process application names, providing a dynamic list of per-app volume sliders to your Lua widgets.

---

### Phase 7: Subprocess PGID Gating & Safe Reload Orchestration

The Supervisor manages the lifecycles of processes spawned by `process.run` with absolute visual and crash safety [oblisk-supervisor-services-dbus.md § 12].

#### Process PGID Gating:
Configure all child forks to spawn within an independent Unix process group (`setsid` or `setpgid` via the `nix` crate). Ensure the command runner maps to this PGID structure:

```rust
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn spawn_pgid_child(cmd: &str, args: &[String]) -> std::io::Result<std::process::Child> {
    unsafe {
        Command::new(cmd)
            .args(args)
            .pre_exec(|| {
                // Establish an independent process group
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                Ok(())
            })
            .spawn()
    }
}
```

#### Safe Reaping Routine:
When a Renderer crash occurs or a hot-reload is triggered, execute the safe cleanup routine:
1.  Locate the active process handle.
2.  Transmit `SIGTERM` to the entire process group: `nix::sys::signal::kill(-pgid, nix::sys::signal::SIGTERM)`.
3.  Spawn a non-blocking 100ms async wait timer. If the child process group has not fully exited, escalate to `SIGKILL` to clean up active screen recorders and input overlays cleanly.

---

### Phase 8: Hot-Reload Presentation Before Authority (PBA) Flow

In `supervisor/src/reload.rs`, orchestrate the overlapping process transitions without introducing a single blank or black frame on the user's display [oblisk-supervisor-services-dbus.md § 15].

```text
Supervisor                            Renderer Gen N                     Renderer Gen N+1 (Candidate)
    │                                       │                                         │
    │── (Spawns Gen N+1) ───────────────────┼────────────────────────────────────────▶│
    │                                       │                                         │ (Binds Wayland / null-buffers)
    │                                       │                                         │ (Stages assets in background)
    │                                       │                                         │
    │◀─ (Ready Event) ──────────────────────┼─────────────────────────────────────────│
    │                                       │                                         │
    │── (ActivateDraw Nonce) ───────────────┼────────────────────────────────────────▶│
    │                                       │                                         │ (Commits GLES3 frames)
    │                                       │                                         │ (Attaches wp_presentation_feedback)
    │                                       │                                         │
    │◀─ (Presented Callback Verified) ──────┼─────────────────────────────────────────│
    │                                       │                                         │
    │── (Clear Input Region) ──────────────▶│                                         │
    │                                       │                                         │
    │── (SIGTERM Group) ───────────────────▶│                                         │
    │                                       │                                         │
    │── (Promote to Active Focus) ──────────┼────────────────────────────────────────▶│
```

1.  **Overlapping Spawn**: Spawn the Candidate `N+1` while keeping the current active generation `N` rendering and receiving input.
2.  **State Hydration**: Immediately write the pre-cached values of NetworkManager, BlueZ, and PipeWire state snapshots down to the Candidate’s control socket, eliminating any startup state-query latency.
3.  **Null-Buffer Staging**: The Candidate completes Wayland Layer-Shell handshakes but commits null graphics buffers, remaining completely invisible.
4.  **Activate Draw**: The Supervisor writes an `ActivateDraw` nonce packet. The Candidate compiles the AST, draws its first layout frame on the GPU, and attaches a `wp_presentation_feedback` request to its commit.
5.  **Evidence Verification**: Once the compositor triggers the `presented` callback, confirming pixels have physically updated on all outputs, the Candidate writes back presentation evidence.
6.  **Swap & Reap**: The Supervisor commands Generation `N` to clear its input region. It marks Generation `N+1` as active, maps its input regions, and reaps the Generation `N` process group cleanly.

---

## 3. Playbook Testing & Validation Protocols

Command your agent to execute this automated shell script workflow to verify structural type checks, socket communication, and AST config parser health:

```bash
#!/usr/bin/env bash
set -euo pipefail

echo "==================================================================="
echo "Oblisk Automated Compiler Testing Playbook"
echo "==================================================================="

# 1. Structural Crate Compile and Workspace Bounds Check
echo "Step 1: Running Cargo Check..."
cargo check --workspace --release

# 2. Emulate Supervisor-Renderer IPC socket handshakes
echo "Step 2: Executing shared IPC serialization tests..."
cargo test -p shared --all-features

# 3. Dry-run the config compiler to ensure Lua AST evaluates properly
echo "Step 3: Validating user layout configuration schema..."
cargo run -p renderer -- --validate ~/.config/oblisk/shell.lua

echo "Success: Scaffolding has compiled with 100% type-safety!"
```
