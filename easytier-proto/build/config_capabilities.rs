//! Configuration extension gates shared by the runtime and frontend codegen.

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use anyhow::{Context as _, bail, ensure};
use prost_reflect::{DescriptorPool, Value};

pub fn write(descriptor_set: &[u8], out: &Path) -> anyhow::Result<()> {
    let pool = DescriptorPool::decode(descriptor_set)?;
    let api = pool
        .get_message_by_name("api.manage.NetworkConfig")
        .context("the compiled schema declares no api.manage.NetworkConfig")?;
    let metadata: BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(include_bytes!("../config-capabilities.json"))?;
    let mut generated = String::from(
        "// Generated from config-capabilities.json. Do not edit.\n\
         const CONFIG_FIELD_CAPABILITIES: &[ConfigFieldCapability] = &[\n",
    );
    for (name, entry) in metadata {
        let field = api
            .get_field_by_name(&name)
            .with_context(|| format!("configuration capability refers to unknown field {name}"))?;
        ensure!(
            !matches!(
                field.options().get_field_by_name("deprecated").as_deref(),
                Some(Value::Bool(true))
            ),
            "configuration capability refers to deprecated field {name}"
        );
        let entry = entry
            .as_object()
            .context("capability entry is not an object")?;
        ensure!(
            entry.len() == 2,
            "unexpected capability metadata for {name}"
        );
        let capability = entry
            .get("capability")
            .and_then(|value| value.as_str())
            .context("capability entry has no capability string")?;
        ensure!(
            capability.starts_with("config:"),
            "invalid capability for {name}"
        );
        let support = match entry.get("support").and_then(|value| value.as_str()) {
            Some("always") => "Always",
            Some("tcp_udp") => "TcpAndUdp",
            Some("tls") => "TlsTransport",
            Some("disguised_p2p") => "DisguisedP2p",
            other => bail!("invalid capability support for {name}: {other:?}"),
        };
        writeln!(
            generated,
            "    ConfigFieldCapability {{ field: {name:?}, capability: {capability:?}, support: ConfigFieldSupport::{support} }},"
        )?;
    }
    generated.push_str("];\n");
    std::fs::write(out.join("config_capabilities.rs"), generated)?;
    Ok(())
}
