use zed_extension_api::lsp::{Completion, CompletionKind};
use zed_extension_api::{
    self as zed, CodeLabel, CodeLabelSpan, Command, LanguageServerId, Result, Worktree,
};

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
            "riscv-asm-lsp not found on PATH; see https://github.com/mihirokawai9472642/riscv-asm-lsp#using-it-with-zed".to_owned()
        })?;
        Ok(Command {
            command: path,
            args: Vec::new(),
            env: Vec::new(),
        })
    }

    fn label_for_completion(
        &self,
        _language_server_id: &LanguageServerId,
        completion: Completion,
    ) -> Option<CodeLabel> {
        match completion.kind {
            Some(CompletionKind::Function | CompletionKind::Snippet | CompletionKind::Keyword) => {
                let len = completion.label.len();
                let filter_end = completion.label.find(char::is_whitespace).unwrap_or(len);
                Some(CodeLabel {
                    spans: vec![CodeLabelSpan::code_range(0..len)],
                    filter_range: (0..filter_end).into(),
                    code: completion.label,
                })
            }
            _ => None,
        }
    }
}

zed::register_extension!(RiscvAssemblyExtension);
