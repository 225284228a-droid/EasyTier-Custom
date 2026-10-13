mod flags;
mod rpc;

use crate::rpc::ServiceGenerator;
use std::{env, path::PathBuf};

#[cfg(target_os = "windows")]
use std::io::Cursor;

#[cfg(target_os = "windows")]
fn check_protoc_exist() -> Option<PathBuf> {
    let path = env::var_os("PROTOC").map(PathBuf::from);
    if path.is_some() && path.as_ref().unwrap().exists() {
        return path;
    }

    let path = env::var_os("PATH").unwrap_or_default();
    for p in env::split_paths(&path) {
        let p = p.join("protoc.exe");
        if p.exists() && p.is_file() {
            return Some(p);
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn get_cargo_target_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let profile = env::var("PROFILE")?;
    let mut target_dir = None;
    let mut sub_path = out_dir.as_path();
    while let Some(parent) = sub_path.parent() {
        if parent.ends_with(&profile) {
            target_dir = Some(parent);
            break;
        }
        sub_path = parent;
    }
    let target_dir = target_dir.ok_or("not found")?;
    Ok(target_dir.to_path_buf())
}

#[cfg(target_os = "windows")]
fn download_protoc() -> PathBuf {
    let out_dir = get_cargo_target_dir().unwrap().join("protobuf");
    let fname = out_dir.join("bin/protoc.exe");
    if fname.exists() {
        println!("cargo:info=use existing protoc: {:?}", fname);
        return fname;
    }

    println!("cargo:info=need download protoc, please wait...");

    let url = "https://github.com/protocolbuffers/protobuf/releases/download/v26.0-rc1/protoc-26.0-rc-1-win64.zip";
    let response = reqwest::blocking::get(url).unwrap();
    println!("{:?}", response);
    let mut content = response
        .bytes()
        .map(|v| v.to_vec())
        .map(Cursor::new)
        .map(zip::ZipArchive::new)
        .unwrap()
        .unwrap();
    content.extract(out_dir).unwrap();

    fname
}

#[cfg(target_os = "windows")]
fn ensure_protoc_for_windows() {
    let protoc_path = if let Some(path) = check_protoc_exist() {
        println!("cargo:info=use os existing protoc: {:?}", path);
        path
    } else {
        download_protoc()
    };

    unsafe {
        env::set_var("PROTOC", protoc_path);
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    ensure_protoc_for_windows();

    let proto_files_reflect = ["proto/peer_rpc.proto", "proto/common.proto"];

    let proto_files = [
        "proto/core_peer.proto",
        "proto/core_config.proto",
        "proto/error.proto",
        "proto/tests.proto",
        "proto/api_instance.proto",
        "proto/api_logger.proto",
        "proto/api_config.proto",
        "proto/api_manage.proto",
        "proto/web.proto",
        "proto/magic_dns.proto",
        "proto/acl.proto",
    ];

    for proto_file in proto_files.iter().chain(proto_files_reflect.iter()) {
        println!("cargo:rerun-if-changed={proto_file}");
    }

    let out = PathBuf::from(env::var("OUT_DIR")?);

    // protoc on Windows cannot write outputs into a non-ASCII OUT_DIR (repo
    // paths may contain non-ASCII characters). Stage descriptor sets in the
    // ASCII temp dir and copy them back after protoc succeeds.
    let descriptor_out = |name: &str| -> (PathBuf, Option<PathBuf>) {
        let dest = out.join(name);
        let needs_staging = dest.to_str().is_none_or(|s| !s.is_ascii());
        if !needs_staging {
            return (dest.clone(), None);
        }
        let staged =
            std::env::temp_dir().join(format!("easytier-proto-{}-{name}", std::process::id()));
        (dest, Some(staged))
    };

    let mut config = prost_build::Config::new();
    if env::var_os("CARGO_FEATURE_JSON_RPC").is_some() {
        config
            .extern_path(".google.protobuf.Any", "::prost_wkt_types::Any")
            .extern_path(".google.protobuf.Timestamp", "::prost_wkt_types::Timestamp")
            .extern_path(".google.protobuf.Value", "::prost_wkt_types::Value");
    } else {
        config
            .extern_path(".google.protobuf.Any", "::prost_types::Any")
            .extern_path(".google.protobuf.Timestamp", "::prost_types::Timestamp")
            .extern_path(".google.protobuf.Value", "::prost_types::Value");
    }
    let (descriptors_dest, descriptors_staged) = descriptor_out("descriptors.bin");
    if let Some(staged) = &descriptors_staged {
        config.file_descriptor_set_path(staged);
    } else {
        config.file_descriptor_set_path(&descriptors_dest);
    }
    config
        .service_generator(Box::new(ServiceGenerator::default()))
        .btree_map(["."])
        .skip_debug([
            ".common.Ipv4Addr",
            ".common.Ipv6Addr",
            ".common.UUID",
            ".api.manage.ManagedCredentialConfig",
            ".api.manage.VpnPortalConfig",
        ])
        .type_attribute(
            ".common.CompressionAlgoPb",
            "#[derive(strum::EnumString, strum::Display)]",
        )
        .type_attribute(
            ".common.CompressionAlgoPb",
            "#[strum(ascii_case_insensitive)]",
        )
        .field_attribute(".common.CompressionAlgoPb.Invalid", "#[strum(disabled)]");

    let mut descriptor_set = config.load_fds(&proto_files, &["proto/"])?;
    if let Some(staged) = &descriptors_staged {
        std::fs::copy(staged, &descriptors_dest)?;
        let _ = std::fs::remove_file(staged);
    }
    // What protoc wrote (this set carries every file, imports included), read
    // before anything else may touch it: the annotations are extension fields,
    // which a prost-types round-trip drops.
    let annotated = std::fs::read(&descriptors_dest)?;

    {
        let common = descriptor_set
            .file
            .iter_mut()
            .find(|file| file.package.as_deref() == Some("common"))
            .unwrap();
        let flags = common
            .message_type
            .iter()
            .find(|message| message.name.as_deref() == Some("Flags"))
            .unwrap();
        config
            .type_attribute(".common.Flags", "#[optionize::optionized]")
            .type_attribute(".common.Flags", "#[optionize(object = FlagsPatch)]");

        let flattened = flags
            .field
            .iter()
            .filter(|field| field.proto3_optional())
            .map(|field| field.name().to_owned());
        for name in flattened {
            config.field_attribute(format!(".common.Flags.{name}"), "#[optionize(flatten)]");
        }

        let mut patch = flags.clone();
        patch.name = Some("FlagsPatch".to_owned());
        // Proto3 spells `optional` as one synthetic one-of per field.
        let oneofs = patch
            .field
            .iter()
            .map(|field| prost_types::OneofDescriptorProto {
                name: Some(format!("_{}", field.name())),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        patch.oneof_decl = oneofs;
        for (index, field) in patch.field.iter_mut().enumerate() {
            field.proto3_optional = Some(true);
            field.label = Some(prost_types::field_descriptor_proto::Label::Optional as i32);
            field.oneof_index = Some(index as i32);
        }
        common.message_type.push(patch);

        config.disable_comments([".common.FlagsPatch"]);
    }

    let descriptors = prost::Message::encode_to_vec(&descriptor_set);

    let (fds_dest, fds_staged) = descriptor_out("file_descriptor_set.bin");
    if let Some(staged) = &fds_staged {
        config.file_descriptor_set_path(staged);
    } else {
        config.file_descriptor_set_path(&fds_dest);
    }
    config.compile_protos(&proto_files_reflect, &["proto/"])?;
    if let Some(staged) = &fds_staged {
        std::fs::copy(staged, &fds_dest)?;
        let _ = std::fs::remove_file(staged);
    }
    config.compile_fds(descriptor_set)?;
    flags::write(&annotated, &out)?;

    pbjson_build::Builder::new()
        .register_descriptors(&descriptors)?
        .preserve_proto_field_names()
        .btree_map(["."])
        .build(&["."])?;

    Ok(())
}
