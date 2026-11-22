fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!(
            "{} {} (MIT; bundled data notices in THIRD_PARTY_NOTICES.md)",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    if let Err(err) = riscv_asm_lsp::server::run_stdio() {
        eprintln!("riscv-asm-lsp fatal error: {err}");
        std::process::exit(1);
    }
}
