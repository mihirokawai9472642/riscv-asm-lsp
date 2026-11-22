use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct RiscvAssemblyExtension;

impl zed::Extension for RiscvAssemblyExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let path = worktree.which("riscv-asm-lsp").ok_or_else(|| {
            "riscv-asm-lsp not found on PATH; install it or configure lsp.riscv-asm-lsp.binary in Zed settings".to_owned()
        })?;
        Ok(Command {
            command: path,
            args: Vec::new(),
            env: Vec::new(),
        })
    }
}

zed::register_extension!(RiscvAssemblyExtension);
