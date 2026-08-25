# Oblisk Test-Driven Development (TDD) Test-Harness Specification
## Heads-Up Test Mocks, Virtual Backends, and Headless Renderer Verification

This document specifies the exact test-harness files, mock objects, and virtual system configurations required to build and compile Oblisk under strict Test-Driven Development (TDD). 

Because the target Linux machine may lack active PipeWire nodes, BlueZ peripherals, or an active Wayland compositor socket (`$WAYLAND_DISPLAY`) during CI/CD or cargo test runs, these specifications detail how to decouple and mock all system, display, and hardware layers.

---

## 1. The TDD Crate Architecture (Test Layout)

To test both Supervisor and Renderer modules in isolation, configure the cargo workspace to support test fixtures and mock frameworks:

```text
oblisk-workspace/
├── shared/
│   └── src/lib.rs
├── supervisor/
│   └── tests/
│       ├── test_dbus_monitors.rs    # Mock zbus and properties testing
│       ├── test_hw_pollers.rs       # Mock sysfs/procfs environment tests
│       └── test_subprocess_group.rs # PGID and zombie process cleanup checks
└── renderer/
    └── tests/
        ├── test_lua_marshalling.rs  # MLua to VirtualNode conversions
        ├── test_layout_solver.rs     # One-pass constraint and flex calculations
        ├── test_damage_tracking.rs   # Pixel snapping and damage union tests
        └── test_headless_renderer.rs # Headless EGL context frame presentation tests
```

---

## 2. Mocking the Hardware & System Layers (Supervisor TDD)

### 2.1 Virtual Sysfs File Test-Harness (`test_hw_pollers.rs`)
Do not hardcode `/sys` or `/proc` absolute file paths in your production code. Instead, pass a runtime environmental variable or configuration struct pointing to a system root baseline (`sys_root`).

#### The Production Rust Loader:
```rust
pub struct SysfsReader {
    sys_root: std::path::PathBuf,
}

impl SysfsReader {
    pub fn new(root: &str) -> Self {
        Self { sys_root: std::path::PathBuf::from(root) }
    }

    pub fn read_battery_capacity(&self) -> Result<u32, std::io::Error> {
        let path = self.sys_root.join("class/power_supply/BAT0/capacity");
        let content = std::fs::read_to_string(path)?;
        content.trim().parse::<u32>().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
```

#### The TDD Assertions:
```rust
#[test]
fn test_battery_capacity_parsing() {
    let temp_dir = tempfile::tempdir().unwrap();
    let bat_dir = temp_dir.path().join("class/power_supply/BAT0");
    std::fs::create_dir_all(&bat_dir).unwrap();
    
    // Test Case 1: Valid capacity parsing
    std::fs::write(bat_dir.join("capacity"), "84\n").unwrap();
    let reader = SysfsReader::new(temp_dir.path().to_str().unwrap());
    assert_eq!(reader.read_battery_capacity().unwrap(), 84);

    // Test Case 2: Corrupted sysfs file throws immediate error boundary
    std::fs::write(bat_dir.join("capacity"), "invalid_data").unwrap();
    assert!(reader.read_battery_capacity().is_err());
}
```

### 2.2 Virtual D-Bus Client Mocks (`test_dbus_monitors.rs`)
To test NetworkManager, BlueZ, and MPRIS properties without binding to the system D-Bus, your singletons must wrap `zbus::Connection` interfaces or leverage mock D-Bus connection servers.

#### The Mock D-Bus Server Setup:
Using `zbus`'s test utilities, instantiate a local, isolated session bus to publish properties, asserting that your monitors receive and serialize changes with zero delays.

```rust
use zbus::{dbus_interface, Connection};

struct MockNetworkManager;

#[dbus_interface(name = "org.freedesktop.NetworkManager")]
impl MockNetworkManager {
    #[dbus_interface(property)]
    fn wireless_enabled(&self) -> bool {
        true
    }
    
    #[dbus_interface(property)]
    fn primary_connection(&self) -> zbus::zvariant::OwnedObjectPath {
        "/org/freedesktop/NetworkManager/ActiveConnection/1".try_into().unwrap()
    }
}

#[tokio::test]
async fn test_networkmanager_properties_parsing() {
    // 1. Spin up an isolated mock D-Bus server
    let connection = Connection::session().await.unwrap();
    connection.object_server().at("/org/freedesktop/NetworkManager", MockNetworkManager).await.unwrap();
    
    // 2. Point production NetworkMonitor to the mock Session Connection
    let monitor = NetworkMonitor::new_with_connection(connection).await.unwrap();
    
    // 3. Assert that properties are reactively synchronized
    assert_eq!(monitor.get_wifi_enabled(), true);
}
```

---

## 3. Headless Presentation & Layout Math Verification (Renderer TDD)

Because Wayland requires a graphics server and `$WAYLAND_DISPLAY` environment variables, graphics tests will crash during standard CI/CD cargo ticks unless you compile a **Headless EGL Presenter**.

### 3.1 Headless EGL Test Harness (`test_headless_renderer.rs`)
Configure your EGL initialization code to fall back to headless Pbuffers (pixel buffers) when physical displays are undetected, allowing FemtoVG rendering passes to execute off-screen.

```rust
use khronos_egl as egl;

pub fn initialize_headless_egl() -> Result<(egl::Display, egl::Context), &'static str> {
    let egl = unsafe { egl::DynamicInstance::load().map_err(|_| "Failed to load EGL library")? };
    
    // Request headless platform extensions
    let display = egl.get_platform_display(
        egl::PLATFORM_SURFACELESS_MESA,
        egl::DEFAULT_DISPLAY,
        &[],
    ).map_err(|_| "Headless presentation unavailable")?;
    
    egl.initialize(display).map_err(|_| "EGL init failed")?;
    
    // Bind OpenGL ES 3 API Context
    let context_attributes = [
        egl::CONTEXT_CLIENT_VERSION, 3,
        egl::NONE,
    ];
    let context = egl.create_context(display, egl::NO_CONFIG, egl::NO_CONTEXT, &context_attributes)
        .map_err(|_| "Context allocation failed")?;
        
    Ok((display, context))
}
```

Using this off-screen context, your tests can execute visual rendering ticks, write the frame output buffer to a flat byte array, and assert color values directly (e.g., confirming that a `rect` with background `#FF0000` writes pure red pixels into the GPU swapchain).

### 3.2 Pure Layout Calculation Unit-Tests (`test_layout_solver.rs`)
To test that your One-Pass Layout Solver handles `row`, `column`, alignment margins, and padding pixel snapping correctly, write isolated unit tests bypassing the Wayland layer entirely:

```rust
#[test]
fn test_row_alignment_distribution_math() {
    // 1. Arrange a horizontal row containing two static elements
    let mut row_container = LayoutNode::new_row(10); // spacing = 10px
    row_container.props.width = SizeConstraint::Pixels(100.0);
    row_container.props.height = SizeConstraint::Pixels(40.0);
    
    let mut child_a = LayoutNode::new_rect("#FF0000");
    child_a.props.width = SizeConstraint::Pixels(30.0);
    child_a.props.height = SizeConstraint::Pixels(20.0);
    child_a.props.align_v = Alignment::Center; // Vertical alignment = Center
    
    let mut child_b = LayoutNode::new_rect("#00FF00");
    child_b.props.width = SizeConstraint::Pixels(40.0);
    child_b.props.height = SizeConstraint::Pixels(30.0);
    
    row_container.add_child(child_a);
    row_container.add_child(child_b);
    
    // 2. Act: Run One-Pass Solver Pass
    let constraints = LayoutConstraints {
        min_width: 0.0, max_width: 100.0,
        min_height: 0.0, max_height: 40.0,
    };
    let resolved = row_container.solve_layout(constraints);
    
    // 3. Assert: Verify solved logical coordinate offsets
    // Total occupied width = 30 (A) + 10 (spacing) + 40 (B) = 80px. Remaining space = 20px.
    assert_eq!(resolved.children[0].geometry.x_offset, 0.0);
    assert_eq!(resolved.children[0].geometry.y_offset, 10.0); // (40 max_h - 20 child_h) / 2 = 10px offset
    
    assert_eq!(resolved.children[1].geometry.x_offset, 40.0); // 30 (A) + 10 (spacing)
    assert_eq!(resolved.children[1].geometry.y_offset, 0.0);  // Top aligned by default
}
```

---

## 4. Headless Lua AST and Signals Verification (Lua VM TDD)

### 4.1 Headless MLua Marshalling Tests (`test_lua_marshalling.rs`)
Test that the Renderer's type marshalling meta-system processes Lua configurations and evaluates nested signals correctly:

```rust
#[test]
fn test_lua_rect_marshalling() {
    let lua = mlua::Lua::new();
    
    // Register visual primitive constructors in global scope
    lua.globals().set("rect", lua.create_function(|_, table: mlua::Table| {
        Ok(table) // Echo table structure back to Rust
    }).unwrap()).unwrap();
    
    // Evaluate mock top-bar layout config
    let chunk = r#"
        return rect {
            id = "test_bar",
            background = "#11111B",
            width = "Fill",
            height = 32
        }
    "#;
    
    let table: mlua::Table = lua.load(chunk).eval().unwrap();
    
    // Assert Rust-side deserialization mapping properties flawlessly
    let node: VirtualNode = deserialize_lua_table(table).unwrap();
    assert_eq!(node.properties.get("id").unwrap(), "test_bar");
    assert_eq!(node.properties.get("background").unwrap(), "#11111B");
    assert_eq!(node.properties.get("height").unwrap(), "32");
}
```

---

## 5. TDD Execution & Assertions Playbook

When instructing your coding agent to develop features, command them to adhere to this test execution progression:

### Step 5.1: Write the Failing Test First
For every feature (e.g. adding CPU temperature reading under `sysinfo`):
1.  Open `supervisor/tests/test_hw_pollers.rs` and write a test case attempting to query a temperature of `45` degrees Celsius from a mock `/sys/class/hwmon/hwmon0/temp1_input` file.
2.  Run the test suite and verify it fails with standard Rust compilation or assertion output:
    ```bash
    cargo test -p supervisor --test test_hw_pollers
    # Output: FAILED (No such file or directory, or method unimplemented)
    ```

### Step 5.2: Implement the Minimal Code to Pass
1.  Implement the sysfs path construction and temperature file reading code block inside `supervisor/src/hardware/sysinfo.rs`.
2.  Run the tests again and assert success:
    ```bash
    cargo test -p supervisor --test test_hw_pollers
    # Output: test_cpu_temp_parsing ... ok
    ```

### Step 5.3: Refactor Under Test Coverage
With the test passing, safely refactor the code (e.g. optimizing string heap allocation inside the file-read parser). If a regression is introduced, the test harness will intercept it instantly.
