# Security

## What the server runs and reads

riscv-asm-lsp reads the files of the workspace it is started in: assembly
files, the headers they include, `compile_commands.json`, `compile_flags.txt`
and `.riscv-asm-lsp.yaml`.

It runs other programs only when `diagnostics.external` is `"auto"` or `"on"`.
It then runs `clang` and `llvm-mc` from the configured paths, `PATH` or the
standard LLVM install directories, with arguments taken from the compilation
database, and never through a shell. The compiler named in
`compile_commands.json` is never run, and `.riscv-asm-lsp.yaml` cannot set
tool paths.

## Reporting a vulnerability

Please do not report security problems in public issues. Use GitHub's
private reporting instead: **Security → Report a vulnerability** on this
repository. Only the latest release receives fixes.
