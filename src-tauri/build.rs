use std::path::PathBuf;
use std::{env, fs};

/// Lists the trusted source-index worker scripts (`bridge/source-index/*.py`, test files excluded) so the app
/// embeds exactly what the checkout contains and never reads the checkout at runtime.
fn embed_source_index_scripts() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let directory = manifest.join("..").join("bridge").join("source-index");
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut names: Vec<String> = fs::read_dir(&directory)
        .unwrap_or_else(|e| {
            panic!(
                "source-index worker scripts are missing at {}: {e}",
                directory.display()
            )
        })
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".py") && !name.starts_with("test"))
        .collect();
    names.sort();
    let mut out = String::from("pub(crate) const SCRIPTS: &[(&str, &str)] = &[\n");
    for name in names {
        let path = directory.join(&name).to_string_lossy().replace('\\', "/");
        out.push_str(&format!("    ({name:?}, include_str!({path:?})),\n"));
    }
    out.push_str("];\n");
    let target =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("source_index_scripts.rs");
    fs::write(target, out).expect("write source index script list");
}

fn main() {
    embed_source_index_scripts();
    tauri_build::build()
}
