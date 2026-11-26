mod asm_manual;
mod fetch;
mod join;
mod llvm;
mod pins;
mod report;
mod udb;

use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workspace_root = workspace_root();

    match args.first().map(String::as_str) {
        Some("fetch") => {
            let paths = fetch::ensure_sources(&workspace_root)?;
            println!(
                "Fetched sources:\n  llvm: {}\n  udb: {}\n  asm-manual: {}",
                paths.llvm_dir.display(),
                paths.udb_dir.display(),
                paths.asm_manual_dir.display()
            );
        }
        Some("gen-data") => {
            let paths = fetch::ensure_sources(&workspace_root)?;
            let llvm_data = llvm::extract(&paths.llvm_dir, &paths.cache_dir)?;
            let udb_data = udb::load(&paths.udb_dir)?;
            let manual_data = asm_manual::load(&paths.asm_manual_dir)?;

            let (data, report_data) =
                join::build(&workspace_root, llvm_data, udb_data, manual_data)?;
            let json_text = serde_json::to_string(&data)?;
            let json_path = workspace_root.join("server/data/riscv.json");
            fs::write(&json_path, format!("{json_text}\n"))?;

            let report_text = report::render(&data, &report_data);
            let report_path = workspace_root.join("server/data/report.md");
            fs::write(&report_path, report_text)?;

            eprintln!(
                "xtask: wrote {} ({} bytes) and {}",
                json_path.display(),
                json_text.len(),
                report_path.display()
            );
        }
        other => {
            return Err(format!("unknown xtask subcommand: {other:?} (expected: gen-data)").into());
        }
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has workspace parent")
        .to_path_buf()
}
