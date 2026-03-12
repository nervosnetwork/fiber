use std::{io::Write, path::PathBuf, str::FromStr};

fn main() {
    let rpc_dir = PathBuf::from_str(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
        .join("../crates/fiber-lib/src/rpc");
    let rpc_files = vec![
        ["cch.rs", "CchRpc"],
        ["channel.rs", "ChannelRpc"],
        ["dev.rs", "DevRpc"],
        ["graph.rs", "GraphRpc"],
        ["info.rs", "InfoRpc"],
        ["invoice.rs", "InvoiceRpc"],
        ["payment.rs", "PaymentRpc"],
        ["peer.rs", "PeerRpc"],
        // ["prof.rs", "ProfRpc"],
        ["watchtower.rs", "WatchtowerRpc"],
    ];
    for line in rpc_files.iter() {
        println!(
            "cargo:rerun-if-changed={}",
            rpc_dir.join(line[0]).as_os_str().to_str().unwrap()
        );
    }
    let output_dir = PathBuf::from_str(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
        .join("src/output");
    std::fs::remove_dir_all(&output_dir).ok();
    std::fs::create_dir_all(&output_dir).unwrap();

    let mut root_mod_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_dir.join("mod.rs"))
        .unwrap();
    let mut rpc_mods = vec![];
    for [file_name, trait_name] in rpc_files {
        let patched_file_name = file_name.replace(".", "_");

        let current_output_dir = output_dir.join(&patched_file_name);
        std::fs::remove_dir_all(&current_output_dir).ok();
        std::fs::create_dir_all(&current_output_dir).unwrap();

        openrpsee::generate_openrpc(
            rpc_dir.join(file_name).to_str().unwrap(),
            &[trait_name],
            false,
            current_output_dir.as_path(),
        )
        .unwrap();
        std::fs::write(current_output_dir.join("mod.rs"), "pub mod rpc_openrpc;").unwrap();
        root_mod_file
            .write(format!("pub mod {};", patched_file_name).as_bytes())
            .unwrap();
        rpc_mods.push(patched_file_name.clone());
        let mut generated_rpc_defs = std::fs::OpenOptions::new()
            .append(true)
            .open(current_output_dir.join("rpc_openrpc.rs"))
            .unwrap();
        generated_rpc_defs
            .write(format!("#[allow(unused_imports)]\nuse fnn::rpc::{}::*;\n#[allow(unused_imports)]\nuse fiber_json_types::*;", file_name.replace(".rs", "")).as_bytes())
            .unwrap();
    }
    root_mod_file.write(format!("pub const API_METHODS: [ (&str, &::phf::Map<&str, openrpsee::openrpc::RpcMethod>); {}] = [",rpc_mods.len()).as_bytes()).unwrap();
    for item in rpc_mods.iter() {
        let tag_name = item.trim_end_matches("_rs");
        root_mod_file
            .write(format!("(\"{}\", &{}::rpc_openrpc::METHODS),", tag_name, item).as_bytes())
            .unwrap();
    }
    root_mod_file.write(b"];").unwrap();
    drop(root_mod_file);
}
