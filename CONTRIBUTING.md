# Contributing to riscv-asm-lsp

riscv-asm-lsp improves in small steps, starting from problems seen on real
code. This file describes how issues and pull requests work.

## Reporting a problem

- Use the **Bug report** form. Give the riscv-asm-lsp version, the editor,
  where you saw the problem (project, file and line, or a minimal snippet),
  and the server log if you can.
- The reference is LLVM 23. For diagnostics and encodings, include what
  `clang` or `llvm-mc` says about the same code.
- Say how the problem was confirmed. Problems seen by a person in an editor,
  or measured on real code, come first. Problems found only on AI-generated
  or synthetic samples need to be reproduced on real code.
- New features wait until the existing ones work well on real projects.
  Questions and ideas go to Discussions.
- Report security problems privately; see `SECURITY.md`.

## Design decisions

When a fix would change documented behaviour or a decision in
`docs/design.md`, open a **Design decision** issue first, and implement it
after the decision is made.

## Making a change

1. Every change starts from an issue.
2. One issue per branch and pull request. Keep it small, with no unrelated
   clean-ups or additions.
3. Branch from an up-to-date `main` and name the branch after the change,
   with the commit type as a prefix (for example `fix/macro-labels`).
   Link the issue in the pull request (`Fixes #123`).
4. Before committing, run the checks listed in `AGENTS.md` (tests, clippy,
   rustfmt, `cargo deny`).
5. A change is done when it has been checked again on the same real code as
   the issue, with the same editor steps or the same measurement. Put the
   results from before and after the change in the pull request. Tests are
   required, but passing them is not enough on its own.
6. Commit messages follow Conventional Commits (see `AGENTS.md`).

## Coding agents

Coding agents may help, but only within the issue at hand. The pull request
says when an agent contributed. Agents follow `AGENTS.md`.
