//! Which node answers a pointer at a point: click, press, drag, wheel and focus targets from one
//! hit path (ADR-0050 decision 1).

use super::super::keyboard::{FieldTarget, focused_field};
use super::*;

/// Innermost node with callable `on_click` or `submits_secure = true` in a hit path (ADR-0050 decision 1).
/// Scan inward: the deepest node is often an unhandled `text` child, and a node without a handler
/// is transparent. `layout::node::resolve_declared` refuses an `on_click` that is not a function.
fn click_target(path: &[&layout::ResolvedNode], point: layout::hit::LogicalPoint, floor: usize) -> Option<Clickable> {
    (floor..path.len()).rev().find_map(|depth| {
        let node = path[depth];
        let on_click = pointer::on_click.read(&node.properties).ok().flatten();
        let submits_secure = pointer::submits_secure.read(&node.properties).is_ok_and(|on| on);
        if on_click.is_none() && !submits_secure {
            return None;
        }
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let at = local_pointer(&path[..=depth], rect, point);
        Some(Clickable { rect, handler: on_click, link: None, submits_secure, at })
    })
}

/// `point` in the untransformed space of `path`'s last node, relative to its box. An empty path or a
/// degenerate transform has no inverse; the raw point minus `rect` then stands, as before transforms.
fn local_pointer(
    path: &[&layout::ResolvedNode],
    rect: LogicalRect,
    point: layout::hit::LogicalPoint,
) -> layout::hit::LogicalPoint {
    layout::hit::node_local(path, point)
        .unwrap_or(layout::hit::LogicalPoint { x: point.x - rect.x, y: point.y - rect.y })
}

/// A held drag's current rect and [`local_pointer`], through the node's current path, so a node
/// that moves under its own drag reports one frame. A node gone from `root` keeps the press rect.
pub(super) fn drag_local(
    root: Option<&layout::ResolvedNode>,
    drag: &PressTarget,
    point: layout::hit::LogicalPoint,
) -> (LogicalRect, layout::hit::LogicalPoint) {
    let path = root.and_then(|root| layout::hit::path_to_node(root, drag.id)).unwrap_or_default();
    let rect = layout::hit::absolute_rect(&path).unwrap_or(drag.rect);
    (rect, local_pointer(&path, rect, point))
}

/// Innermost node with callable `handler` (`on_press`, `on_double_click` or `on_drag`); unhandled nodes are
/// transparent, like `on_click`'s scan, and each handler has its own target.
pub(super) fn press_target(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    floor: usize,
    handler: fn(&layout::ResolvedNode) -> Option<Function>,
) -> Option<PressTarget> {
    (floor..path.len()).rev().find_map(|depth| {
        let node = path[depth];
        let handler = handler(node)?;
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        Some(PressTarget { rect, handler, at: local_pointer(&path[..=depth], rect, point), id: node.id })
    })
}

fn on_press(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_press.read(&node.properties).ok().flatten()
}

/// `on_drag` (ADR-0116 decision 1): unhandled nodes are transparent, so a handle inside a
/// draggable track leaves the track draggable.
pub(super) fn on_drag(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_drag.read(&node.properties).ok().flatten()
}

fn on_double_click(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_double_click.read(&node.properties).ok().flatten()
}

/// Click, press and double-click targets for a hit. A `textfield` is a leaf that answers only to
/// its own handlers, never an ancestor's (ADR-0092 decision 7); a masked one answers to none.
fn handlers_under(
    path: &[&layout::ResolvedNode],
    field: Option<&FieldTarget>,
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> (Option<Clickable>, Option<PressTarget>, Option<PressTarget>) {
    let floor = match field {
        None => 0,
        // A textfield is always the deepest hit node, so the leaf is the field.
        Some(FieldTarget::Plain { .. }) => path.len().saturating_sub(1),
        Some(FieldTarget::Masked { .. }) => return (None, None, None),
    };
    let click = if field.is_none() { clickable(path, point, shaping) } else { click_target(path, point, floor) };
    (click, press_target(path, point, floor, on_press), press_target(path, point, floor, on_double_click))
}

/// Innermost node with callable `on_wheel` (ADR-0116 decision 2).
pub(super) fn wheel_target(path: &[&layout::ResolvedNode]) -> Option<(usize, LogicalRect, Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        let on_wheel = pointer::on_wheel.read(&node.properties).ok().flatten()?;
        Some((depth, layout::hit::absolute_rect(&path[..=depth])?, on_wheel))
    })
}

/// The `on_press`, `on_double_click` or `on_drag` node under the pointer, with its rect and the
/// node-local pointer. For a drag, `id` is re-found each event so the pointer is mapped through
/// the node's current transform.
#[derive(Clone)]
pub(super) struct PressTarget {
    pub(super) rect: LogicalRect,
    pub(super) handler: Function,
    pub(super) at: layout::hit::LogicalPoint,
    pub(super) id: layout::scene::NodeId,
}

/// Press/release target: handler, click-identity rect (see [`ArmedClick`]), and link `href`.
#[derive(Clone)]
pub(super) struct Clickable {
    pub(super) rect: LogicalRect,
    pub(super) handler: Option<Function>,
    pub(super) link: Option<String>,
    /// `submits_secure = true` sends the scope's armed `secure_submit` field on release, like Enter
    /// (ADR-0114); it is the only pointer path to a password, with no Lua callback.
    pub(super) submits_secure: bool,
    /// Pointer in the node's own coordinates ([`local_pointer`]).
    pub(super) at: layout::hit::LogicalPoint,
}

/// Links precede `on_click` (ADR-0106): a text `on_link` with an `href` under `point` wins over any
/// `on_click`, the text's own included, while plain words pass through. A `textfield` similarly
/// arms a click only for non-left buttons (ADR-0092 decision 7).
pub(super) fn clickable(
    path: &[&layout::ResolvedNode],
    point: layout::hit::LogicalPoint,
    shaping: &ShapingHandle,
) -> Option<Clickable> {
    let link = path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "text" {
            return None;
        }
        let on_link = node::fields::text_flow::on_link.read(&node.properties).ok().flatten()?;
        let rect = layout::hit::absolute_rect(&path[..=depth])?;
        let local = layout::hit::node_local(&path[..=depth], point)?;
        let href = layout::hit::link_under(node, local, shaping)?;
        Some(Clickable { rect, handler: Some(on_link), link: Some(href), submits_secure: false, at: local })
    });
    link.or_else(|| click_target(path, point, 0))
}

/// Both targets from decision 1's single [`layout::hit::hit_path`] traversal. Two walks could
/// re-resolve between them and give one event two answers.
#[derive(Default)]
pub(super) struct PointerHit {
    pub(super) click: Option<Clickable>,
    pub(super) field: Option<FieldTarget>,
    pub(super) control: Option<layout::scene::NodeId>,
    /// Byte offset under the point in the plain `textfield` it landed in (ADR-0236), decided on
    /// this same walk so one event cannot get two answers.
    pub(super) caret: Option<usize>,
    /// `on_drag` node under the press, with its rect (ADR-0116 decision 1).
    pub(super) drag: Option<PressTarget>,
    pub(super) press: Option<PressTarget>,
    pub(super) double: Option<PressTarget>,
}

/// A masked field, or the `submits_secure` node that sends one.
fn hits_a_secret(field: Option<&FieldTarget>, click: Option<&Clickable>) -> bool {
    matches!(field, Some(FieldTarget::Masked { .. })) || click.is_some_and(|click| click.submits_secure)
}

fn focusable_hit(path: &[&layout::ResolvedNode]) -> Option<layout::scene::NodeId> {
    path.iter()
        .rev()
        .find(|node| layout::scene::is_named_control(node) || focused_field(&[node]).is_some())
        .map(|node| node.id)
}

impl App {
    /// One hit-test answers click, field, and drag (ADR-0050 decisions 1/4). Coordinates are
    /// logical surface-local while `paint_surface` remains scale `1.0`; a future HiDPI change must
    /// move this conversion with `paint_surface` and `apply_input_region`. Handlers and targets are
    /// cloned out of the lent tree; the tree itself is not.
    pub(super) fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit::default();
        };
        let point = layout::hit::LogicalPoint::from_position(position);
        let path = layout::hit::hit_path(tree, point);
        let field = focused_field(&path);
        // `None`, not `""`, for a field this does not hold: an empty draft measures to offset 0,
        // and handing that to `drag_selection` moves the held field's head into another field's
        // text. Only the draft `press_chooses_focus` would resume has a caret (ADR-0108).
        let (click, press, double) = handlers_under(&path, field.as_ref(), point, &self.shaping);
        let held = self
            .focused_text_field
            .as_ref()
            .filter(|held| matches!(&field, Some(FieldTarget::Plain { id, .. }) if *id == held.id));
        PointerHit {
            click,
            control: focusable_hit(&path),
            // Its own caret too, not just its draft: a draft too wide to fit is drawn slid to
            // follow the caret, and a press reads the byte under where it actually landed.
            caret: held
                .and_then(|held| layout::hit::caret_at(&path, point, &held.buffer, held.selection.1, &self.shaping)),
            field,
            drag: press_target(&path, point, 0, on_drag),
            press,
            double,
        }
    }

    /// Whether a press or release at `position` would reach a masked field or a `submits_secure` node, which
    /// `mantle input` refuses to touch.
    pub(in crate::wayland::input) fn reaches_a_secret(&self, index: usize, position: (f64, f64)) -> bool {
        let hit = self.hit_under(index, position);
        hits_a_secret(hit.field.as_ref(), hit.click.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::pt;
    use super::*;
    use crate::wayland::input::keyboard::tests::{plain_textfield, secure_submit_table, textfield, with_property};
    use crate::wayland::input::tests::hit_node;
    use mlua::Table;

    #[test]
    fn pointer_focus_chooses_a_field_or_named_button_from_the_hit_path() {
        let lua = Lua::new();
        let field = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let mut button = hit_node(&lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
        std::rc::Rc::make_mut(&mut button.properties)
            .insert("accessible_name", Value::String(lua.create_string("Unlock").unwrap()));
        let unnamed = hit_node(&lua, "rect", (0.0, 0.0, 30.0, 20.0), true);
        assert_eq!(focusable_hit(&[&field]), Some(field.id));
        assert_eq!(focusable_hit(&[&button]), Some(button.id));
        assert_eq!(focusable_hit(&[&button, &unnamed]), Some(button.id));
        assert_eq!(focusable_hit(&[&unnamed]), None);
    }

    #[test]
    fn a_masked_field_or_a_submit_node_is_a_secret_and_a_plain_field_is_not() {
        let lua = Lua::new();
        let point = pt(1.0, 1.0);
        let masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        assert!(hits_a_secret(focused_field(&[&masked]).as_ref(), None));
        let plain = plain_textfield(&lua);
        assert!(!hits_a_secret(focused_field(&[&plain]).as_ref(), None));
        let submit =
            with_property(hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), false), "submits_secure", Value::Boolean(true));
        let click = click_target(&[&submit], point, 0).expect("a submit node is clickable");
        assert!(click.handler.is_none(), "submit alone arms a click, with no Lua handler to call (ADR-0114)");
        assert!(hits_a_secret(None, Some(&click)));
        let inert = hit_node(&lua, "rect", (0.0, 0.0, 9.0, 9.0), true);
        assert!(!hits_a_secret(None, click_target(&[&inert], point, 0).as_ref()));
    }

    #[test]
    fn the_innermost_on_drag_or_on_wheel_node_takes_it_and_a_bare_node_is_transparent() {
        let lua = Lua::new();
        let handler = || Value::Function(lua.create_function(|_, ()| Ok(())).unwrap());
        let mut thumb = hit_node(&lua, "icon", (5.0, 2.0, 20.0, 20.0), true);
        std::rc::Rc::make_mut(&mut thumb.properties).insert("on_wheel", handler());
        let mut track = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut track.properties).insert("on_drag", handler());
        std::rc::Rc::make_mut(&mut track.properties).insert("on_wheel", handler());
        track.children.push(thumb);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(track);

        let on_thumb = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let press_at = layout::hit::LogicalPoint { x: 20.0, y: 12.0 };
        let PressTarget { rect, .. } =
            press_target(&on_thumb, press_at, 0, on_drag).expect("the track carries the on_drag");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
        let (depth, rect, _) = wheel_target(&on_thumb).expect("the thumb carries an on_wheel");
        assert_eq!((depth, rect), (2, LogicalRect { x: 15.0, y: 6.0, width: 20.0, height: 20.0 }), "innermost");
        let beside = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 45.0, y: 12.0 });
        assert_eq!(wheel_target(&beside).map(|(depth, ..)| depth), Some(1), "the track, past the thumb");
        let bare = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        assert!(press_target(&[&bare], press_at, 0, on_drag).is_none() && wheel_target(&[&bare]).is_none());
    }

    #[test]
    fn on_press_takes_the_innermost_handler_and_gets_rect_button_and_local_pointer() {
        let lua = Lua::new();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let handler = Value::Function(
            lua.create_function(move |_, (rect, button, pointer): (Table, String, Table)| {
                sink.borrow_mut().push((rect.get::<f32>("width").unwrap(), button, pointer.get::<f32>("x").unwrap()));
                Ok(())
            })
            .unwrap(),
        );
        let mut bar = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut bar.properties).insert("on_press", handler);
        bar.children.push(hit_node(&lua, "icon", (5.0, 2.0, 20.0, 20.0), true));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(bar);

        let at = layout::hit::LogicalPoint { x: 20.0, y: 12.0 };
        let path = layout::hit::hit_path(&root, at);
        let target =
            press_target(&path, at, 0, on_press).expect("the bar carries on_press; its icon child is transparent");
        call_on_press(&lua, &target.handler, &target, "right", [false; 4]).unwrap();
        assert_eq!(*seen.borrow(), vec![(40.0, "right".to_string(), 10.0)]);
        let bare = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        assert!(press_target(&[&bare], at, 0, on_press).is_none());
    }

    #[test]
    fn the_innermost_handled_node_of_any_kind_under_the_pointer_is_the_one_that_would_fire() {
        // The shape decision 1 exists for: the deepest node is an unhandled `text`, and the outer
        // `column` has no handler, so only the `row` between them answers.
        let lua = Lua::new();
        let mut row = hit_node(&lua, "row", (10.0, 4.0, 40.0, 24.0), true);
        row.children.push(hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), false));
        let mut column = hit_node(&lua, "column", (0.0, 0.0, 100.0, 32.0), false);
        column.children.push(row);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(column.clone());

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let Clickable { rect, .. } = click_target(&path, layout::hit::LogicalPoint { x: 20.0, y: 12.0 }, 0)
            .expect("the row carries an on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });

        // The same `text` leaf with its own `on_click` is now the innermost handler.
        column.children[0].children[0] = hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), true);
        root.children[0] = column;
        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let Clickable { rect, .. } = click_target(&path, layout::hit::LogicalPoint { x: 20.0, y: 12.0 }, 0)
            .expect("the text carries an on_click");
        assert_eq!(rect, LogicalRect { x: 16.0, y: 9.0, width: 28.0, height: 14.0 });
    }

    /// `pointer` is where the node's own box was hit, whatever its drawn transform.
    #[test]
    fn on_click_pointer_is_node_local_through_translate_scale_and_rotate() {
        use crate::wayland::node::apply_affine;
        let lua = Lua::new();
        type Case = (&'static str, fn(&mut layout::ResolvedNode));
        let cases: [Case; 3] = [
            ("translate", |n| n.transform.translate = (1785.0, 0.0)),
            ("scale", |n| n.transform.scale = (2.0, 2.0)),
            ("rotate", |n| n.transform.rotate = 90.0),
        ];
        for (name, transform) in cases {
            let mut node = hit_node(&lua, "rect", (10.0, 4.0, 40.0, 24.0), true);
            transform(&mut node);
            let mut root = hit_node(&lua, "panel", (0.0, 0.0, 3000.0, 400.0), false);
            root.children.push(node);
            // The node's own point (15, 4) in its box, found where it is drawn.
            let drawn = apply_affine(layout::hit::path_transform(&[&root, &root.children[0]]), 25.0, 8.0);
            let point = layout::hit::LogicalPoint { x: drawn.0, y: drawn.1 };
            let path = layout::hit::hit_path(&root, point);
            let Clickable { at, .. } = click_target(&path, point, 0).expect(name);
            assert!((at.x - 15.0).abs() < 1e-3 && (at.y - 4.0).abs() < 1e-3, "{name}: {}, {}", at.x, at.y);
        }
    }

    /// A held drag maps through the node's path now and reports its current rect, not the
    /// press-time one; with no path the raw point minus the press `rect` stands.
    #[test]
    fn on_drag_pointer_follows_the_node_and_falls_back_to_the_raw_point() {
        let lua = Lua::new();
        let build = |mutate: fn(&mut layout::ResolvedNode)| {
            let mut node = hit_node(&lua, "rect", (10.0, 4.0, 40.0, 24.0), true);
            mutate(&mut node);
            let id = node.id;
            let mut root = hit_node(&lua, "panel", (0.0, 0.0, 3000.0, 400.0), false);
            root.children.push(node);
            (root, id)
        };
        let drag = |id| PressTarget {
            rect: LogicalRect { x: 2.0, y: 4.0, width: 40.0, height: 24.0 },
            handler: lua.create_function(|_, ()| Ok(())).unwrap(),
            at: pt(0.0, 0.0),
            id,
        };
        let point = pt(25.0, 8.0);
        type Case = (&'static str, fn(&mut layout::ResolvedNode), layout::hit::LogicalPoint);
        let cases: [Case; 3] = [
            ("moved since the press", |_| {}, pt(15.0, 4.0)),
            ("translated", |n| n.transform.translate = (100.0, 0.0), pt(-85.0, 4.0)),
            ("degenerate transform", |n| n.transform.scale = (0.0, 0.0), pt(15.0, 4.0)),
        ];
        let now = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        for (name, mutate, want) in cases {
            let (root, id) = build(mutate);
            assert_eq!(drag_local(Some(&root), &drag(id), point), (now, want), "{name}");
        }
        let (root, _) = build(|_| {});
        let gone = drag(layout::scene::NodeId::test(u64::MAX));
        let pressed = (drag(gone.id).rect, pt(23.0, 4.0));
        assert_eq!(drag_local(Some(&root), &gone, point), pressed, "removed mid-drag");
        assert_eq!(drag_local(None, &gone, point), pressed, "surface gone");
    }

    #[test]
    fn on_double_click_and_on_click_take_their_own_innermost_targets() {
        let lua = Lua::new();
        let mut icon = hit_node(&lua, "icon", (0.0, 0.0, 20.0, 20.0), false);
        let properties = std::rc::Rc::make_mut(&mut icon.properties);
        properties.insert("on_double_click", handler(&lua));
        properties.insert("accessible_name", Value::String(lua.create_string("Open").unwrap()));
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 40.0, 24.0), false);
        std::rc::Rc::make_mut(&mut row.properties).insert("on_click", handler(&lua));
        let point = layout::hit::LogicalPoint { x: 1.0, y: 1.0 };
        let path = [&row, &icon];
        let click = click_target(&path, point, 0).expect("the row's on_click");
        assert_eq!(click.rect.width, 40.0, "the icon does not swallow the row's single click");
        let double = press_target(&path, point, 0, on_double_click).expect("the icon's on_double_click");
        assert_eq!(double.id, icon.id);
        let shaping = ShapingHandle::spawn();
        assert_eq!(layout::hit::cursor_under(&[&icon], point, &shaping), cursor_icon::CursorIcon::Pointer);
        assert!(!layout::scene::is_named_control(&icon), "keyboard activation never double-clicks");
    }

    fn handler(lua: &Lua) -> Value {
        Value::Function(lua.create_function(|_, ()| Ok(())).unwrap())
    }

    /// A card with both handlers around `field`, which gets its own pair when `own`.
    fn card_around(lua: &Lua, field: layout::ResolvedNode, own: bool) -> (layout::ResolvedNode, layout::ResolvedNode) {
        let card = with_property(
            with_property(hit_node(lua, "column", (0.0, 0.0, 99.0, 99.0), false), "on_click", handler(lua)),
            "on_press",
            handler(lua),
        );
        let field = if own {
            with_property(with_property(field, "on_click", handler(lua)), "on_press", handler(lua))
        } else {
            field
        };
        (card, field)
    }

    #[test]
    fn a_plain_field_answers_to_its_own_handlers_and_never_a_cards_and_a_masked_one_to_none() {
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let at = pt(5.0, 5.0);
        let (card, own) = card_around(&lua, plain_textfield(&lua), true);
        let field = focused_field(&[&own]);
        let (click, press, _) = handlers_under(&[&card, &own], field.as_ref(), at, &shaping);
        assert_eq!(
            click.expect("the field's on_click").rect,
            LogicalRect { x: 0.0, y: 0.0, width: 40.0, height: 24.0 }
        );
        assert!(press.is_some_and(|press| press.rect.width == 40.0), "the field's on_press, not the card's");

        let (card, bare) = card_around(&lua, plain_textfield(&lua), false);
        let field = focused_field(&[&bare]);
        let (click, press, _) = handlers_under(&[&card, &bare], field.as_ref(), at, &shaping);
        assert!(click.is_none() && press.is_none(), "no own handler: the card's never fires");
        // Outside a field the card still wins, as before.
        let (click, press, _) = handlers_under(&[&card], None, at, &shaping);
        assert!(click.is_some() && press.is_some());

        let masked = textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")));
        let (card, masked) = card_around(&lua, masked, true);
        let field = focused_field(&[&masked]);
        let (click, press, _) = handlers_under(&[&card, &masked], field.as_ref(), at, &shaping);
        assert!(click.is_none() && press.is_none());
    }
}
