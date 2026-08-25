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

Establish the compilation parameters in the workspace root. We enforce stable Rust edition 2021, Rust 1.78+, and highly aggressive production profile configurations to eliminate garbage collection stutters in hot paths.

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
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
thiserror = "1.0"
```

#### 3. `supervisor/Cargo.toml`
```toml
[package]
name = "supervisor"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "1.35", features = ["full"] }
zbus = { version = "3.14", features = ["tokio"] }
tokio-stream = "0.1"
futures-util = "0.3"
nix = { version = "0.27", features = ["process", "signal"] }
regex = "1.10"
```

#### 4. `renderer/Cargo.toml`
```toml
[package]
name = "renderer"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../shared" }
tokio = { version = "1.35", features = ["rt", "net", "macros"] }
mlua = { version = "0.9", features = ["lua54", "vendored"] }
wayland-client = "0.31"
wayland-protocols-wlr = { version = "0.2", features = ["client"] }
wayland-protocols = { version = "0.31", features = ["client"] }
khronos_egl = { version = "6.0", features = ["static"] }
gl = "0.14"
femtovg = "0.9"
cosmic-text = "0.11"
```

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
1.  **Registry Binding**: Bind the core Wayland objects (`wl_compositor`, `wl_subcompositor`) and the layer-shell protocol (`zwlr_layer_shell_v1` or stable `ext-layer-shell-v1`).
2.  **EGL Context Allocation**: Initialize EGL with `khronos_egl` [SCTK EGL Module](https://github.com/Smithay/client-toolkit/tree/main/src/egl). Find the optimal EGL config supporting 8-bit ARGB color formats (`EGL_SURFACE_TYPE` with `EGL_WINDOW_BIT`, `EGL_RENDERABLE_TYPE` with `EGL_OPENGL_ES3_BIT`).
3.  **Surface Context Binding**: Match the Wayland physical native window (`wl_egl_window`) to the created EGL surface and make the GLES3 rendering context current on the thread. Refer to `noctalia`'s OpenGL ES Renderer initialization logic: [Noctalia Renderer Setup](https://github.com/noctalia-dev/noctalia/tree/main/src/renderer).
4.  **Static Layer Constraints**: Register the two static surfaces returned by your layout:
    *   `main_bar`: anchored on top, marked exclusive.
    *   `overlay_canvas`: anchored to all four edges, non-exclusive, transparent. On boot, immediately commit an empty input region (`wl_compositor::create_region` with no added coordinates) to allow background applications to receive pointer clicks.

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
