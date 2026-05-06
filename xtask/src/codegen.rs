//! `cargo xtask codegen` — generate TypeScript bindings for the
//! stax-live RPC services into `frontend/src/generated/`.

use std::error::Error;
use std::path::PathBuf;

pub fn run() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    let ts_dir = workspace_root
        .join("frontend")
        .join("src")
        .join("generated");
    std::fs::create_dir_all(&ts_dir)?;

    let services: Vec<_> = stax_live_proto::all_services();

    for service in &services {
        let ts = vox_codegen::targets::typescript::generate_service(service);
        let ts_filename = format!("{}.generated.ts", service.service_name.to_lowercase());
        write_if_changed(&ts_dir.join(&ts_filename), ts)?;
    }

    Ok(())
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is xtask/, so the workspace root is its parent.
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap())
        .parent()
        .unwrap()
        .to_path_buf()
}

fn write_if_changed(
    path: &std::path::Path,
    contents: impl AsRef<[u8]>,
) -> Result<(), Box<dyn Error>> {
    let contents = contents.as_ref();
    if std::fs::read(path).ok().as_deref() == Some(contents) {
        println!("Unchanged {}", path.display());
        return Ok(());
    }
    std::fs::write(path, contents)?;
    println!("Wrote {}", path.display());
    Ok(())
}
