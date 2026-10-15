#![cfg(feature = "core")]

use std::collections::{BTreeMap, BTreeSet};

use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Kind, Value};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorSet};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Field {
    message: String,
    field: String,
    number: u32,
    #[serde(rename = "type")]
    kind: i32,
    type_name: String,
    label: i32,
    proto3_optional: bool,
    #[serde(default)]
    previous_number: Option<u32>,
    #[serde(default)]
    retained: bool,
}

#[derive(Deserialize)]
struct Manifest {
    fields: Vec<Field>,
}

fn manifests() -> (Vec<Field>, Vec<Field>) {
    let official: Manifest = serde_json::from_str(include_str!("upstream-fields.json")).unwrap();
    let custom: Manifest = serde_json::from_str(include_str!("custom-fields.json")).unwrap();
    (official.fields, custom.fields)
}

fn descriptor_fields() -> BTreeMap<String, FieldDescriptorProto> {
    fn visit(
        message: &DescriptorProto,
        prefix: &str,
        result: &mut BTreeMap<String, FieldDescriptorProto>,
    ) {
        let name = format!("{prefix}.{}", message.name());
        if !message
            .options
            .as_ref()
            .is_some_and(|options| options.map_entry())
        {
            for field in &message.field {
                result.insert(format!("{name}.{}", field.name()), field.clone());
            }
        }
        for nested in &message.nested_type {
            visit(nested, &name, result);
        }
    }
    let descriptors = FileDescriptorSet::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
    let mut result = BTreeMap::new();
    for file in descriptors.file {
        if file.package().starts_with("google.protobuf") {
            continue;
        }
        for message in &file.message_type {
            visit(message, file.package(), &mut result);
        }
    }
    result
}

#[test]
fn every_field_has_a_recorded_owner_and_wire_shape() {
    let (official, custom) = manifests();
    let official: BTreeMap<_, _> = official
        .iter()
        .map(|field| (format!("{}.{}", field.message, field.field), field))
        .collect();
    let custom: BTreeMap<_, _> = custom
        .iter()
        .map(|field| (format!("{}.{}", field.message, field.field), field))
        .collect();
    for (name, descriptor) in descriptor_fields() {
        let recorded = official
            .get(&name)
            .or_else(|| custom.get(&name))
            .unwrap_or_else(|| {
                panic!("{name}: record ownership and allocate Custom fields above 50000")
            });
        assert_eq!(descriptor.number() as u32, recorded.number, "{name}");
        assert_eq!(
            descriptor.r#type.unwrap_or_default(),
            recorded.kind,
            "{name}"
        );
        assert_eq!(descriptor.type_name(), recorded.type_name, "{name}");
        assert_eq!(
            descriptor.label.unwrap_or_default(),
            recorded.label,
            "{name}"
        );
        assert_eq!(
            descriptor.proto3_optional(),
            recorded.proto3_optional,
            "{name}"
        );
        if custom.contains_key(&name) {
            assert!(descriptor.number() >= 50000, "{name}: Custom low tag");
        }
    }
    let descriptors = descriptor_fields();
    for (name, field) in &official {
        assert_eq!(
            descriptors.contains_key(name),
            field.retained,
            "official field presence: {name}"
        );
    }
    for name in custom.keys() {
        assert!(
            descriptors.contains_key(name),
            "unrecorded Custom removal: {name}"
        );
    }
}

#[test]
fn custom_allocations_follow_the_migration_rule() {
    let (official, custom) = manifests();
    let mut occupied: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
    for field in &official {
        occupied
            .entry(&field.message)
            .or_default()
            .insert(field.number);
    }
    for field in &custom {
        if field.previous_number.unwrap() >= 50000 {
            occupied
                .entry(&field.message)
                .or_default()
                .insert(field.number);
        }
    }
    let mut sorted: Vec<_> = custom.iter().collect();
    sorted.sort_by_key(|field| (&field.message, field.previous_number));
    for field in sorted {
        let old = field.previous_number.unwrap();
        if old >= 50000 {
            assert_eq!(field.number, old);
            continue;
        }
        let used = occupied.entry(&field.message).or_default();
        let mut expected = old + 50000;
        while used.contains(&expected) {
            expected += 1000;
        }
        assert_eq!(field.number, expected, "{}.{}", field.message, field.field);
        used.insert(expected);
    }
}

fn sample(kind: Kind) -> Value {
    match kind {
        Kind::Bool => Value::Bool(true),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(7),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(7),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(7),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(7),
        Kind::Float => Value::F32(1.5),
        Kind::Double => Value::F64(1.5),
        Kind::String => Value::String("custom-value".into()),
        Kind::Bytes => Value::Bytes(vec![1, 2, 3].into()),
        Kind::Enum(kind) => Value::EnumNumber(kind.values().last().unwrap().number()),
        Kind::Message(kind) => Value::Message(DynamicMessage::new(kind)),
    }
}

#[test]
fn every_custom_field_round_trips_on_its_high_tag() {
    let pool = DescriptorPool::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
    for field in manifests().1 {
        let descriptor = pool.get_message_by_name(&field.message).unwrap();
        let slot = descriptor.get_field_by_name(&field.field).unwrap();
        let mut message = DynamicMessage::new(descriptor.clone());
        let value = sample(slot.kind());
        let value = if slot.is_list() {
            Value::List(vec![value])
        } else {
            value
        };
        message.set_field(&slot, value.clone());
        let bytes = message.encode_to_vec();
        assert!(!bytes.is_empty(), "{}.{}", field.message, field.field);
        let decoded = DynamicMessage::decode(descriptor, bytes.as_slice()).unwrap();
        assert_eq!(
            decoded.get_field(&slot).as_ref(),
            &value,
            "{}.{}",
            field.message,
            field.field
        );
    }
}

#[cfg(feature = "api")]
#[test]
fn upstream_dhcp_and_custom_policy_do_not_share_tags() {
    use easytier_proto::api::manage::NetworkConfig;
    let config = NetworkConfig {
        dhcp_ipv4_cidr: Some("10.55.0.0/24".into()),
        dhcpv6: Some(true),
        dhcp_ipv6_cidr: Some("fd55::/64".into()),
        sni: Some("example.com".into()),
        only_use_wss_http3_for_hole_punching: Some(true),
        prefer_wss_http3_for_p2p: Some(false),
        disable_wss_http3_for_p2p: Some(false),
        ..Default::default()
    };
    assert_eq!(
        NetworkConfig::decode(config.encode_to_vec().as_slice()).unwrap(),
        config
    );
    let pool = DescriptorPool::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
    let descriptor = pool
        .get_message_by_name("api.manage.NetworkConfig")
        .unwrap();
    for (name, number) in [
        ("dhcp_ipv4_cidr", 74),
        ("dhcpv6", 75),
        ("dhcp_ipv6_cidr", 76),
        ("only_use_wss_http3_for_hole_punching", 50074),
        ("prefer_wss_http3_for_p2p", 50075),
        ("disable_wss_http3_for_p2p", 50076),
    ] {
        assert_eq!(descriptor.get_field_by_name(name).unwrap().number(), number);
    }
}
