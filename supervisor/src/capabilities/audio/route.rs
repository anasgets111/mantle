//! ALSA hardware route pod construction and extraction.

use std::collections::HashMap;

use pipewire::spa::pod::{Object, Property, Value};
use pipewire::spa::sys as spa_sys;

use super::master::{RawSinkProps, extract_sink_props, props_object_with_id};

/// Builds the `SPA_PARAM_Route` object for a hardware sink. `index` selects the card route and
/// `profile_device` matches the sink's `card.profile.device` to `Route.device`. `save: true`
/// matches other mixer writes and survives re-plug; volume/mute ride in nested `Props` (the live
/// `pw-cli enum-params <device> Route` shape).
pub(super) fn route_object(
    index: i32,
    profile_device: i32,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) -> Value {
    Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
        id: spa_sys::SPA_PARAM_Route,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(profile_device)),
            Property::new(
                spa_sys::SPA_PARAM_ROUTE_props,
                props_object_with_id(spa_sys::SPA_PARAM_Route, channel_volumes, muted),
            ),
            Property::new(spa_sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    })
}

/// A device's active `Route`: its write index, optional port, and authoritative mixer Props.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ActiveRoute {
    pub index: i32,
    pub port: Option<String>,
    pub props: Option<RawSinkProps>,
}

/// Pulls `(card.profile.device, active route)` from a published `Route`. Cards advertise several
/// routes, and the wrong index writes nowhere. `None` skips objects missing either field, including
/// `EnumRoute`-shaped or otherwise unrelated params.
///
/// A Route whose Props lack `mute` keeps the mute `routes` holds for `(device_id, card.profile.device)`.
pub(super) fn extract_route_target(
    value: &Value,
    routes: &HashMap<(u32, i32), ActiveRoute>,
    device_id: u32,
) -> Option<(i32, ActiveRoute)> {
    let Value::Object(object) = value else { return None };
    let (mut index, mut profile_device, mut port, mut props_value) = (None, None, None, None);
    for property in &object.properties {
        match (property.key, &property.value) {
            (spa_sys::SPA_PARAM_ROUTE_index, Value::Int(value)) => index = Some(*value),
            (spa_sys::SPA_PARAM_ROUTE_device, Value::Int(value)) => profile_device = Some(*value),
            (spa_sys::SPA_PARAM_ROUTE_props, value) => props_value = Some(value),
            // `[count, key, value, key, value, ...]`
            (spa_sys::SPA_PARAM_ROUTE_info, Value::Struct(info)) => {
                for pair in info.get(1..).unwrap_or_default().chunks(2) {
                    if let [Value::String(key), Value::String(value)] = pair
                        && key == "port.type"
                    {
                        port = Some(value.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let profile_device = profile_device?;
    let previous_mute =
        routes.get(&(device_id, profile_device)).and_then(|route| route.props.as_ref()).is_some_and(|props| props.mute);
    let props = props_value.and_then(|value| extract_sink_props(value, previous_mute));
    Some((profile_device, ActiveRoute { index: index?, port, props }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_without_mute_keeps_the_stored_mute() {
        let stored = ActiveRoute {
            index: 2,
            port: None,
            props: Some(RawSinkProps { mute: true, channel_volumes: vec![0.1], channel_map: vec![] }),
        };
        let routes = std::collections::HashMap::from([((5, 7), stored)]);
        let object = route_object(2, 7, Some(vec![0.027]), None);

        let (_, route) = extract_route_target(&object, &routes, 5).unwrap();

        assert!(route.props.unwrap().mute);
    }

    #[test]
    fn a_route_object_round_trips_back_to_the_target_it_names() {
        let object = route_object(2, 7, Some(vec![0.027, 0.027]), Some(true));
        assert_eq!(
            extract_route_target(&object, &Default::default(), 0),
            Some((
                7,
                ActiveRoute {
                    index: 2,
                    port: None,
                    props: Some(RawSinkProps { mute: true, channel_volumes: vec![0.027, 0.027], channel_map: vec![] }),
                }
            ))
        );
    }

    #[test]
    fn extract_route_target_reads_the_port_type_from_the_route_info() {
        let string = |text: &str| Value::String(text.to_string());
        let route = Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
            id: spa_sys::SPA_PARAM_Route,
            properties: vec![
                Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(3)),
                Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(4)),
                Property::new(
                    spa_sys::SPA_PARAM_ROUTE_info,
                    Value::Struct(vec![
                        Value::Int(2),
                        string("device.icon_name"),
                        string("audio-headphones"),
                        string("port.type"),
                        string("headphones"),
                    ]),
                ),
            ],
        });
        let expected = ActiveRoute { index: 3, port: Some("headphones".to_string()), props: None };
        assert_eq!(extract_route_target(&route, &Default::default(), 0), Some((4, expected)));
    }

    #[test]
    fn a_route_object_asks_for_the_setting_to_be_saved() {
        let Value::Object(route) = route_object(2, 7, Some(vec![0.5]), None) else {
            panic!("route_object must build an object")
        };
        let save = route
            .properties
            .iter()
            .find(|property| property.key == spa_sys::SPA_PARAM_ROUTE_save)
            .expect("a Route must carry save");
        assert_eq!(save.value, Value::Bool(true));
    }

    #[test]
    fn extract_route_target_skips_an_object_missing_either_field() {
        let index_only = Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
            id: spa_sys::SPA_PARAM_Route,
            properties: vec![Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(2))],
        });
        assert_eq!(extract_route_target(&index_only, &Default::default(), 0), None);
        assert_eq!(extract_route_target(&Value::Bool(true), &Default::default(), 0), None);
    }
}
