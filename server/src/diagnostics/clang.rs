use lsp_types::Uri;

use crate::context::{FileContext, uri_to_path};
use crate::tools::ToolRunner;

use super::StoredDiagnostic;
use super::parse::parse_clang_output_with_ctx;

pub(super) fn run_clang_diagnostics(
    tools: &ToolRunner,
    uri: &Uri,
    text: &str,
    file_ctx: &FileContext,
) -> Option<Vec<StoredDiagnostic>> {
    let path = uri_to_path(uri);

    let mut args = file_ctx.clang_flags.clone();
    if let Some(dir) = path.as_deref().and_then(|p| p.parent()) {
        args.push("-iquote".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.push("-x".to_owned());
    args.push(if file_ctx.preprocessed {
        "assembler-with-cpp".to_owned()
    } else {
        "assembler".to_owned()
    });
    args.extend([
        "-c".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-".to_owned(),
    ]);

    let output = tools.run_clang(&args, text, file_ctx.working_dir.as_deref())?;
    Some(parse_clang_output_with_ctx(
        text,
        &output.stderr,
        path.as_deref(),
        Some(uri),
        Some(file_ctx),
    ))
}
