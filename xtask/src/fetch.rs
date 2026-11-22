use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::pins;

pub struct SourcePaths {
    pub llvm_dir: PathBuf,
    pub udb_dir: PathBuf,
    pub asm_manual_dir: PathBuf,
    pub cache_dir: PathBuf,
}

pub fn ensure_sources(workspace_root: &Path) -> Result<SourcePaths, Box<dyn std::error::Error>> {
    let cache_dir = workspace_root.join(".cache").join("xtask");
    fs::create_dir_all(&cache_dir)?;

    let llvm_dir = cache_dir.join("llvm-project");
    let udb_dir = cache_dir.join("riscv-unified-db");
    let asm_manual_dir = cache_dir.join("riscv-asm-manual");

    ensure_llvm_sparse(&llvm_dir, pins::LLVM_REPO, pins::LLVM_TAG)?;
    ensure_git_commit(&udb_dir, pins::UDB_REPO, pins::UDB_COMMIT)?;
    ensure_git_commit(
        &asm_manual_dir,
        pins::ASM_MANUAL_REPO,
        pins::ASM_MANUAL_COMMIT,
    )?;

    Ok(SourcePaths {
        llvm_dir,
        udb_dir,
        asm_manual_dir,
        cache_dir,
    })
}

fn ensure_llvm_sparse(dir: &Path, repo: &str, tag: &str) -> Result<(), Box<dyn std::error::Error>> {
    let sparse_patterns = [
        "sparse-checkout",
        "set",
        "--no-cone",
        "/llvm/include/llvm/**/*.td",
        "/llvm/lib/Target/RISCV/**/*.td",
        "/llvm/LICENSE.TXT",
    ];
    if is_at_rev(dir, tag) {
        run_git(Some(dir), &sparse_patterns)?;
        return Ok(());
    }
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    eprintln!("xtask: fetching LLVM sparse checkout ({tag})...");
    run_git(
        None,
        &[
            "clone",
            "--depth=1",
            "--filter=blob:none",
            "--sparse",
            "--branch",
            tag,
            repo,
            &dir.to_string_lossy(),
        ],
    )?;
    run_git(Some(dir), &sparse_patterns)?;
    Ok(())
}

fn ensure_git_commit(
    dir: &Path,
    repo: &str,
    commit: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_at_prefix(dir, commit) {
        return Ok(());
    }
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    eprintln!("xtask: fetching {repo} at {commit}...");
    run_git(
        None,
        &[
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            repo,
            &dir.to_string_lossy(),
        ],
    )?;
    run_git(Some(dir), &["checkout", "--detach", commit])?;
    Ok(())
}

fn is_at_rev(dir: &Path, rev: &str) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    let Ok(head) = git_output(dir, &["rev-parse", "HEAD"]) else {
        return false;
    };
    let Ok(target) = git_output(dir, &["rev-parse", &format!("{rev}^{{commit}}")]) else {
        return false;
    };
    head.trim() == target.trim()
}

fn is_at_prefix(dir: &Path, prefix: &str) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    let Ok(head) = git_output(dir, &["rev-parse", "HEAD"]) else {
        return false;
    };
    head.trim().starts_with(prefix)
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("git {:?} exited with status {status}", args).into());
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("git").current_dir(cwd).args(args).output()?;
    if !out.status.success() {
        return Err(format!("git {:?} failed", args).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
