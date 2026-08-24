# Oblisk Layout Engine & Geometry Specification
## Version 1.0.0

This specification details the mathematical, algorithmic, and programmatic contracts governing the Oblisk Layout Engine. The layout engine is implemented entirely in compiled Rust within the ephemeral Renderer's scene-graph module, utilizing the `cosmic-text` library for glyph shaping and layout. It exposes a minimal, declarative node vocabulary to the Lua VM.

To prevent LLMs and developers from hardcoding visual products (such as bars, menus, or launchers) in Rust, the engine rejects high-level widgets. It operates strictly as a one-pass geometric constraint solver that resolves primitive nodes into scaled physical damage rectangles.

---

## 1. The Minimal Node Vocabulary

The Rust retained-scene graph (`SceneGraph`) accepts only the following primitive nodes. Any compound desktop component must be composed entirely in Lua using these primitives.

```
                  +-----------------------------------+
                  |            SceneGraph             |
                  +-----------------------------------+
                                    |
            +-----------------------+-----------------------+
            |                                               |
  +------------------+                            +------------------+
  |  Container Nodes |                            |   Leaf Nodes     |
  +------------------+                            +------------------+
  |  - Panel         |                            |  - Text          |
  |  - Row           |                            |  - Icon          |
  |  - Column        |                            |  - Rect          |
  |                  |                            |  - TextField *   |
  +------------------+                            +------------------+
                                                    * (Engine exception)
```

### 1.1 Structural Nodes
1.  **`Panel`**: A generic bounding container that supports absolute positioning, clipping, backgrounds, and stacked children.
2.  **`Row`**: A horizontal layout flex-container that distributes children along the X-axis.
3.  **`Column`**: A vertical layout flex-container that distributes children along the Y-axis.

### 1.2 Leaf Nodes
4.  **`Text`**: A read-only text container driven by `cosmic-text`.
5.  **`Icon`**: A raster/vector icon container that pulls from system themes or path URIs.
6.  **`Rect`**: A primitive colored box supporting solid colors, gradients, borders, and independent corner rounding.
7.  **`TextField`** (*Engine Exception*): A interactive, multi-line/single-line text input field. This is the only interactive leaf node implemented in Rust to bridge Wayland's `wp-text-input-v3` and the system clipboard without exposing keystrokes to the Lua heap.

---

## 2. Geometric Core & Layout Data Structures

All calculations use single-precision floating-point coordinates (`f32`) representing logical pixels. They are converted to integer physical pixels (`i32`) only during final viewport projection.

### 2.1 Rust Data Structures

The Rust layout interface enforces the following definitions:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thickness {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Default for Thickness {
    fn default() -> Self {
        Self { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeConstraint {
    Pixels(f32),
    Percent(f32), // 0.0 to 1.0 of parent container's inner allocation
    Content,      // Shrink-wrap to fit child bounds
    Fill,         // Stretch to consume maximum available parent space
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutConstraints {
    pub min_width: f32,
    pub max_width: f32,
    pub min_height: f32,
    pub max_height: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutResult {
    pub width: f32,
    pub height: f32,
    pub x_offset: f32,
    pub y_offset: f32,
}
```

---

## 3. The One-Pass Layout Algorithm

Oblisk rejects the expensive multi-pass layout trees and cyclic binding graphs used in QtQuick/QML. Layout calculations occur in a single, top-down-bottom-up-top-down pass executed on every scene tick.

```
[Constraint Pass] (Top-Down)
      │  Parent passes maximum dimensions minus margins/padding
      ▼
[Size Resolution Pass] (Bottom-Up)
      │  Leaf nodes resolve size (e.g., text measurements via cosmic-text)
      ▼
[Position & Stretch Pass] (Top-Down)
         Parent distributes spare space and aligns coordinates
```

### 3.1 Step 1: The Constraint Pass (Top-Down)
The parent container computes the available bounding dimensions for its children. For a parent with bounds $W_{max} \times H_{max}$, padding $P$, and child margin $M$:

$$\text{Inner } W_{avail} = W_{max} - (P.left + P.right) - (M.left + M.right)$$

$$\text{Inner } H_{avail} = H_{max} - (P.top + P.bottom) - (M.top + M.bottom)$$

These bounds are clamped by the child's explicit properties:

```rust
fn compute_constraints(
    parent_avail_w: f32,
    parent_avail_h: f32,
    child_width: SizeConstraint,
    child_height: SizeConstraint,
    limits: LayoutConstraints,
) -> LayoutConstraints {
    let mut target_min_w = limits.min_width;
    let mut target_max_w = limits.max_width.min(parent_avail_w);
    let mut target_min_h = limits.min_height;
    let mut target_max_h = limits.max_height.min(parent_avail_h);

    match child_width {
        SizeConstraint::Pixels(p) => {
            target_min_w = p;
            target_max_w = p;
        }
        SizeConstraint::Percent(frac) => {
            let computed = parent_avail_w * frac.clamp(0.0, 1.0);
            target_min_w = computed;
            target_max_w = computed;
        }
        _ => {}
    }

    match child_height {
        SizeConstraint::Pixels(p) => {
            target_min_h = p;
            target_max_h = p;
        }
        SizeConstraint::Percent(frac) => {
            let computed = parent_avail_h * frac.clamp(0.0, 1.0);
            target_min_h = computed;
            target_max_h = computed;
        }
        _ => {}
    }

    LayoutConstraints {
        min_width: target_min_w,
        max_width: target_max_w,
        min_height: target_min_h,
        max_height: target_max_h,
    }
}
```

### 3.2 Step 2: Size Resolution Pass (Bottom-Up)
Leaf nodes compute their intrinsic size bounds.

#### Text Node Size Resolution
Text measurement is completed using `cosmic-text`. The layout engine initializes a static, shared font database (`fontdb`) and a text buffer:

```rust
fn measure_text(
    content: &str,
    font_size: f32,
    line_height: f32,
    max_width: f32,
    buffer: &mut cosmic_text::Buffer,
) -> (f32, f32) {
    buffer.set_size(max_width, f32::MAX);
    buffer.set_text(content, cosmic_text::Attrs::new(), cosmic_text::Shaping::Advanced);
    
    // Iterate over shaped lines to compute structural bounds
    let mut computed_width: f32 = 0.0;
    let mut computed_height: f32 = 0.0;

    for line in buffer.lines.iter() {
        if let Some(ref layout) = line.layout_opt() {
            for glyph_line in layout.iter() {
                computed_width = computed_width.max(glyph_line.w);
                computed_height += line_height;
            }
        }
    }

    (computed_width, computed_height)
}
```

#### Row & Column Size Accumulation
*   **Row Intrinsic Size**:
    $$W_{row} = \sum_{i=1}^{N} W_{child, i} + (N-1) \times \text{spacing}$$
    
    $$H_{row} = \max_{i=1 \dots N} (H_{child, i})$$

*   **Column Intrinsic Size**:
    $$W_{col} = \max_{i=1 \dots N} (W_{child, i})$$
    
    $$H_{col} = \sum_{i=1}^{N} H_{child, i} + (N-1) \times \text{spacing}$$

### 3.3 Step 3: Position & Stretch Resolution Pass (Top-Down)
Once sizes are resolved, the parent container calculates final offsets. It distributes remaining space ($S_{spare} = \text{Parent Inner Allocation} - \text{Total Child Intrinsic}$) based on vertical and horizontal alignment properties.

#### Row Layout Allocation Loop
For a `Row` container stretching horizontal space:

```rust
fn resolve_row_positions(
    children: &mut [Node],
    parent_inner_w: f32,
    parent_inner_h: f32,
    spacing: f32,
) {
    let mut stretch_count = 0;
    let mut total_fixed_width = 0.0;

    for child in children.iter() {
        if child.props.align_h == Alignment::Stretch {
            stretch_count += 1;
        } else {
            total_fixed_width += child.geometry.width + child.props.margin.left + child.props.margin.right;
        }
    }
    
    total_fixed_width += spacing * (children.len().saturating_sub(1)) as f32;

    let remaining_w = (parent_inner_w - total_fixed_width).max(0.0);
    let stretch_allocation = if stretch_count > 0 {
        remaining_w / stretch_count as f32
    } else {
        0.0
    };

    let mut current_x = 0.0;
    for child in children.iter_mut() {
        let margin = child.props.margin;
        current_x += margin.left;

        if child.props.align_h == Alignment::Stretch {
            child.geometry.width = stretch_allocation - (margin.left + margin.right);
        }

        // Apply alignment along cross-axis (Vertical)
        let cross_spare = parent_inner_h - child.geometry.height - margin.top - margin.bottom;
        let child_y_offset = match child.props.align_v {
            Alignment::Start => margin.top,
            Alignment::Center => margin.top + (cross_spare * 0.5),
            Alignment::End => parent_inner_h - child.geometry.height - margin.bottom,
            Alignment::Stretch => {
                child.geometry.height = parent_inner_h - (margin.top + margin.bottom);
                margin.top
            }
        };

        child.geometry.x_offset = current_x;
        child.geometry.y_offset = child_y_offset;

        current_x += child.geometry.width + margin.right + spacing;
    }
}
```

---

## 4. Keyed Reconciliation Algorithm (The Diff & Patch Engine)

To prevent screen stutter and performance degradation when Lua lists rebuild, the Rust engine utilizes a stable, linear-key matching algorithm. It does not destroy the physical scene nodes on updates; it patches properties and preserves active animations.

```
Lua Virtual Node List:   [ A (key: 1) ] -> [ B (key: 2) ] -> [ C (key: 3) ]
                                              |
                                              v (Linear Key Scan & Reconciliation)
                                              |
Rust Active Nodes:       [ A (key: 1) ] -> [ D (key: 4) ] -> [ B (key: 2) ]
                                              |
                                              v (Result)
Preserved & Repositioned: [ A ] & [ B ] (animations intact)
New Node Spawned:         [ C ]
Destroyed Node Reaped:    [ D ] (clean GPU texture cleanup)
```

```rust
pub struct VirtualNode {
    pub key: Option<String>,
    pub kind: String,
    pub properties: std::collections::HashMap<String, String>,
    pub children: Vec<VirtualNode>,
}

pub struct ActiveNode {
    pub key: Option<String>,
    pub id: u64, // Stable hardware identifier
    pub state_hash: u64,
    pub raw_node: Node,
    pub children: Vec<ActiveNode>,
}

impl SceneGraph {
    pub fn reconcile(
        &mut self,
        active_list: &mut Vec<ActiveNode>,
        v_list: Vec<VirtualNode>,
    ) {
        let mut reconciled_nodes = Vec::with_capacity(v_list.len());

        for (v_index, v_node) in v_list.into_iter().enumerate() {
            // Step 1: Linear search to match existing keys
            let matched_active_idx = if let Some(ref v_key) = v_node.key {
                active_list.iter().position(|a| a.key.as_ref() == Some(v_key))
            } else {
                // If no key is set, fall back to positional match of matching node types
                active_list.iter().enumerate().position(|(idx, a)| {
                    a.key.is_none() && a.raw_node.kind == v_node.kind && idx == v_index
                })
            };

            if let Some(idx) = matched_active_idx {
                // Match found: Remove from active pool and patch properties
                let mut active_node = active_list.remove(idx);
                
                // Compute state hash changes
                let new_hash = calculate_state_hash(&v_node.properties);
                if active_node.state_hash != new_hash {
                    active_node.raw_node.update_properties(v_node.properties);
                    active_node.state_hash = new_hash;
                }

                // Recursively reconcile children
                self.reconcile(&mut active_node.children, v_node.children);
                reconciled_nodes.push(active_node);
            } else {
                // No match found: Instantiate new scene node
                let mut new_node = ActiveNode {
                    key: v_node.key.clone(),
                    id: self.generate_unique_id(),
                    state_hash: calculate_state_hash(&v_node.properties),
                    raw_node: Node::new_from_virtual(v_node.kind, &v_node.properties),
                    children: Vec::new(),
                };
                self.reconcile(&mut new_node.children, v_node.children);
                reconciled_nodes.push(new_node);
            }
        }

        // Step 2: Remaining nodes in active_list are orphaned. Harvest and clean up GPU resources
        for orphaned_node in active_list.drain(..) {
            self.reclaim_node_gpu_resources(orphaned_node);
        }

        *active_list = reconciled_nodes;
    }
}
```

---

## 5. Fractional Scaling and Subpixel Snapping

To ensure crisp layout lines and prevent blurry text rendering under fractional desktop scaling (e.g., HiDPI screens configured at 125% or 150% scales), Oblisk implements a strict physical scaling and subpixel snapping protocol.

### 5.1 The Fractional Scaling Contract
1.  **Logical Computation**: The one-pass layout solver computes all positions in standard floating-point logical pixels.
2.  **Physical Projection**: Before pushing dirty bounding boxes to the EGL viewport or generating rendering damage regions, coordinates are multiplied by the output's fractional scaling factor $S_f$ (e.g., $S_f = 1.25$):

$$X_{phys} = \text{round}(X_{logical} \times S_f)$$

$$Y_{phys} = \text{round}(Y_{logical} \times S_f)$$

$$W_{phys} = \lceil (X_{logical} + W_{logical}) \times S_f \rceil - X_{phys}$$

$$H_{phys} = \lceil (Y_{logical} + H_{logical}) \times S_f \rceil - Y_{phys}$$

By using ceiling bounds on the outer edges and rounding on inner edges, Oblisk prevents visual subpixel gaps and overlapping borders between adjacent elements.

### 5.2 Viewport Snapping Rules
*   **Borders & Outlines**: Solid colors and container borders must snap exactly to single-pixel alignments. A logical border width of `1.0` at a scale of `1.25` yields a physical width of `1.0` (using floor calculations), preventing fuzzy, anti-aliased border lines.
*   **Text Alignments**: Font metrics are calculated using integer physical point sizes. The layout engine rounds line-height markers to the nearest physical integer boundary.

---

## 6. Layout-to-Render Damage Verification

To keep idle CPU/GPU consumption minimal, the layout engine does not repaint the entire canvas on changes.

1.  **Damage Accumulation**: When a node is updated or reconciled, its physical bounding box is pushed into a `DamageTracker` queue.
2.  **Region Merging**: Before dispatching frames via `wl_surface::damage_buffer`, overlapping damage bounding rectangles are merged using standard intersection clipping algorithms to minimize paint operations.
3.  **Frame Boundaries**: If the combined damage region is empty, the Renderer bypasses FemtoVG drawing completely, releasing the system CPU and GPU.
