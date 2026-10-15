use std::{fs, path::PathBuf};

/// Compile every `proto/invest/v1/*.proto` into Rust with tonic — BOTH client and server
/// stubs. invest-core includes the servers; a consumer (the invest BFF) the clients. The
/// generated module is pulled into `src/lib.rs` via `tonic::include_proto!("invest.v1")`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
	let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");

	let dir = proto_root.join("invest/v1");
	println!("cargo:rerun-if-changed={}", dir.display());
	let mut protos: Vec<PathBuf> = Vec::new();
	for entry in fs::read_dir(&dir)? {
		let path = entry?.path();
		if path.extension().is_some_and(|ext| ext == "proto") {
			println!("cargo:rerun-if-changed={}", path.display());
			protos.push(path);
		}
	}
	protos.sort();

	tonic_prost_build::configure().build_server(true).build_client(true).compile_protos(&protos, &[proto_root])?;

	Ok(())
}
