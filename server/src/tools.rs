use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{MessageType, ShowMessageParams};

use crate::diagnostics::MessageSink;

const LLVM_INSTALL_URL: &str = "https://github.com/mihirokawai9472642/riscv-asm-lsp#optional-llvm-for-clang-based-diagnostics-and-expansions";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Clang,
    LlvmMc,
}

impl ToolKind {
    fn base_name(self) -> &'static str {
        match self {
            Self::Clang => "clang",
            Self::LlvmMc => "llvm-mc",
        }
    }

    fn exe_name(self) -> &'static str {
        match self {
            Self::Clang => "clang.exe",
            Self::LlvmMc => "llvm-mc.exe",
        }
    }

    fn setting_name(self) -> &'static str {
        match self {
            Self::Clang => "clang",
            Self::LlvmMc => "llvmMc",
        }
    }

    fn feature_name(self) -> &'static str {
        match self {
            Self::Clang => "diagnostics",
            Self::LlvmMc => "hover expansions",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Default)]
struct ToolSlot {
    explicit: Option<String>,
    probed: bool,
    resolved_bin: Option<String>,
    disabled: bool,
    runtime_logged: bool,
    timeout_logged: bool,
}

impl ToolSlot {
    fn new(explicit: Option<String>) -> Self {
        Self {
            explicit,
            ..Self::default()
        }
    }

    fn reset(&mut self, explicit: Option<String>) {
        *self = Self::new(explicit);
    }
}

pub struct ToolRunner {
    clang: Mutex<ToolSlot>,
    llvm_mc: Mutex<ToolSlot>,
    message_sink: Option<MessageSink>,
}

impl ToolRunner {
    pub fn new(
        clang_bin: Option<String>,
        llvm_mc_bin: Option<String>,
        message_sink: Option<MessageSink>,
    ) -> Self {
        Self {
            clang: Mutex::new(ToolSlot::new(clang_bin)),
            llvm_mc: Mutex::new(ToolSlot::new(llvm_mc_bin)),
            message_sink,
        }
    }

    pub fn update_bins(&self, clang_bin: Option<String>, llvm_mc_bin: Option<String>) {
        if let Ok(mut slot) = self.clang.lock()
            && slot.explicit != clang_bin
        {
            slot.reset(clang_bin);
        }
        if let Ok(mut slot) = self.llvm_mc.lock()
            && slot.explicit != llvm_mc_bin
        {
            slot.reset(llvm_mc_bin);
        }
    }

    pub fn run_clang(
        &self,
        args: &[String],
        stdin_text: &str,
        working_dir: Option<&Path>,
    ) -> Option<ToolOutput> {
        let bin = self.resolve_bin(ToolKind::Clang)?;
        self.run(
            ToolKind::Clang,
            &bin,
            args,
            stdin_text,
            working_dir,
            Duration::from_secs(5),
        )
    }

    pub fn run_llvm_mc(&self, args: &[String], stdin_text: &str) -> Option<ToolOutput> {
        let bin = self.resolve_bin(ToolKind::LlvmMc)?;
        self.run(
            ToolKind::LlvmMc,
            &bin,
            args,
            stdin_text,
            None,
            Duration::from_secs(2),
        )
    }

    pub fn is_disabled(&self, kind: ToolKind) -> bool {
        self.slot(kind).lock().map(|s| s.disabled).unwrap_or(false)
    }

    fn slot(&self, kind: ToolKind) -> &Mutex<ToolSlot> {
        match kind {
            ToolKind::Clang => &self.clang,
            ToolKind::LlvmMc => &self.llvm_mc,
        }
    }

    fn resolve_bin(&self, kind: ToolKind) -> Option<String> {
        let mut slot = self.slot(kind).lock().ok()?;
        if !slot.probed {
            self.probe_slot(kind, &mut slot);
        }
        if slot.disabled {
            return None;
        }
        slot.resolved_bin.clone()
    }

    fn probe_slot(&self, kind: ToolKind, slot: &mut ToolSlot) {
        slot.probed = true;
        let candidates = candidate_bins(kind, slot.explicit.as_deref());
        let is_explicit = slot.explicit.is_some();

        let mut last_err: Option<String> = None;

        for cand in &candidates {
            if !is_explicit && looks_like_path(cand) && !Path::new(cand).is_file() {
                continue;
            }
            let version_out = match Command::new(cand).arg("--version").output() {
                Ok(out) if out.status.success() => combine_output(&out.stdout, &out.stderr),
                Ok(out) => {
                    last_err = Some(format!("`{cand} --version` exited with {}", out.status));
                    continue;
                }
                Err(err) => {
                    last_err = Some(format!("failed to execute `{cand}`: {err}"));
                    continue;
                }
            };

            if supports_riscv_target(kind, cand, &version_out) {
                slot.resolved_bin = Some(cand.clone());
                return;
            }
            last_err = Some(format!("`{cand}` does not include riscv32/riscv64 targets"));
        }

        slot.disabled = true;
        let requested = slot.explicit.as_deref().unwrap_or_else(|| kind.base_name());
        let detail = last_err.unwrap_or_else(|| "executable not found".to_owned());
        eprintln!(
            "riscv-asm-lsp: disabling {} ({requested}): {detail}",
            kind.feature_name()
        );

        // Only notify the user via LSP when an explicitly configured binary fails.
        // Auto-discovery falling short is normal when no external toolchain is installed.
        if is_explicit {
            let msg = format!(
                "RISC-V Assembly: {} disabled — configured `{requested}` unusable ({detail}). Check `lsp.riscv-asm-lsp.settings.{}` or see {LLVM_INSTALL_URL}",
                kind.feature_name(),
                kind.setting_name(),
            );
            self.notify_warning(&msg);
        }
    }

    fn run(
        &self,
        kind: ToolKind,
        bin: &str,
        args: &[String],
        stdin_text: &str,
        working_dir: Option<&Path>,
        timeout: Duration,
    ) -> Option<ToolOutput> {
        if self.is_disabled(kind) {
            return None;
        }

        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = working_dir
            && dir.is_dir()
        {
            cmd.current_dir(dir);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                let permanent = matches!(
                    err.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::InvalidInput
                );
                if permanent {
                    self.disable_once(kind, bin, &format!("failed to execute `{bin}`: {err}"));
                } else {
                    self.log_transient_once(
                        kind,
                        bin,
                        &format!("temporary error spawning `{bin}`: {err}"),
                    );
                }
                return None;
            }
        };

        let stdin_bytes = stdin_text.as_bytes().to_vec();
        let writer = child.stdin.take().map(|mut pipe| {
            thread::spawn(move || {
                let _ = pipe.write_all(&stdin_bytes);
            })
        });

        let stdout_reader = child.stdout.take().map(|mut pipe| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                buf
            })
        });

        let stderr_reader = child.stderr.take().map(|mut pipe| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                buf
            })
        });

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };

        if let Some(h) = writer {
            let _ = h.join();
        }
        let stdout_buf = stdout_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr_buf = stderr_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();

        if timed_out {
            self.log_timeout_once(kind, bin, timeout);
            return None;
        }

        let status = status?;
        Some(ToolOutput {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        })
    }

    fn log_transient_once(&self, kind: ToolKind, bin: &str, reason: &str) {
        if let Ok(mut slot) = self.slot(kind).lock()
            && !slot.runtime_logged
        {
            slot.runtime_logged = true;
            eprintln!("riscv-asm-lsp: {bin}: {reason}");
        }
    }

    fn log_timeout_once(&self, kind: ToolKind, bin: &str, timeout: Duration) {
        if let Ok(mut slot) = self.slot(kind).lock()
            && !slot.timeout_logged
        {
            slot.timeout_logged = true;
            eprintln!(
                "riscv-asm-lsp: `{bin}` timed out after {}s during {}",
                timeout.as_secs(),
                kind.feature_name()
            );
        }
    }

    fn disable_once(&self, kind: ToolKind, bin: &str, reason: &str) {
        let should_log = if let Ok(mut slot) = self.slot(kind).lock() {
            slot.disabled = true;
            let first = !slot.runtime_logged;
            slot.runtime_logged = true;
            first
        } else {
            false
        };
        if should_log {
            eprintln!(
                "riscv-asm-lsp: disabling {} ({bin}): {reason}",
                kind.feature_name()
            );
        }
    }

    fn notify_warning(&self, message: &str) {
        let Some(sink) = &self.message_sink else {
            return;
        };
        let params = ShowMessageParams {
            typ: MessageType::WARNING,
            message: message.to_owned(),
        };
        if let Ok(val) = serde_json::to_value(params) {
            sink(Message::Notification(Notification::new(
                "window/showMessage".to_owned(),
                val,
            )));
        }
    }
}

pub(crate) struct SystemRoots<'a> {
    pub linux_usr_lib: &'a Path,
    pub homebrew_opt: &'a Path,
    pub usr_local_opt: &'a Path,
    pub windows_llvm_bin: &'a Path,
}

impl Default for SystemRoots<'static> {
    fn default() -> Self {
        Self {
            linux_usr_lib: Path::new("/usr/lib"),
            homebrew_opt: Path::new("/opt/homebrew/opt"),
            usr_local_opt: Path::new("/usr/local/opt"),
            windows_llvm_bin: Path::new(r"C:\Program Files\LLVM\bin"),
        }
    }
}

/// Ordered tool candidates for auto-discovery or explicit configuration.
pub fn candidate_bins(kind: ToolKind, explicit: Option<&str>) -> Vec<String> {
    let path_var = std::env::var_os("PATH");
    discover_candidates(
        kind,
        explicit,
        path_var.as_deref(),
        &SystemRoots::default(),
        cfg!(windows),
    )
}

pub(crate) fn discover_candidates(
    kind: ToolKind,
    explicit: Option<&str>,
    path_var: Option<&OsStr>,
    roots: &SystemRoots<'_>,
    is_windows: bool,
) -> Vec<String> {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return vec![p.to_owned()];
    }

    let bare = kind.base_name();
    let exe = kind.exe_name();
    let mut out = vec![bare.to_owned()];
    let mut seen: HashSet<String> = HashSet::from([bare.to_owned()]);

    if let Some(path_os) = path_var {
        for name in collect_versioned_on_path(bare, is_windows, path_os) {
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }

    for path in collect_standard_install_bins(bare, exe, roots, is_windows) {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }

    out
}

fn collect_versioned_on_path(bare: &str, is_windows: bool, path_os: &OsStr) -> Vec<String> {
    let prefix = format!("{bare}-");
    let mut found: Vec<(u32, String)> = Vec::new();
    for dir in std::env::split_paths(path_os) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name_os) = entry.file_name().into_string().ok() else {
                continue;
            };
            if let Some(ver) = parse_versioned_tool_name(&name_os, &prefix, is_windows)
                && entry.path().is_file()
            {
                found.push((ver, name_os));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, name)| name).collect()
}

fn parse_versioned_tool_name(name: &str, prefix: &str, is_windows: bool) -> Option<u32> {
    let stem = if is_windows {
        name.strip_suffix(".exe").unwrap_or(name)
    } else {
        name
    };
    let rest = stem.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn collect_standard_install_bins(
    bare: &str,
    exe: &str,
    roots: &SystemRoots<'_>,
    is_windows: bool,
) -> Vec<String> {
    let mut out = Vec::new();

    if is_windows {
        let candidate = roots.windows_llvm_bin.join(exe);
        if candidate.is_file() || roots.windows_llvm_bin == Path::new(r"C:\Program Files\LLVM\bin")
        {
            out.push(candidate.to_string_lossy().into_owned());
        }
        return out;
    }

    // Linux distro packages: /usr/lib/llvm-<N>/bin/<tool>, newest major first.
    for dir in sorted_versioned_subdirs(roots.linux_usr_lib, "llvm-") {
        let bin = dir.join("bin").join(bare);
        if bin.is_file() {
            out.push(bin.to_string_lossy().into_owned());
        }
    }

    // macOS Homebrew formulas: unversioned `llvm` then versioned `llvm@<N>`, newest first.
    for opt_root in [roots.homebrew_opt, roots.usr_local_opt] {
        let unversioned = opt_root.join("llvm").join("bin").join(bare);
        if unversioned.is_file() {
            out.push(unversioned.to_string_lossy().into_owned());
        }
        for dir in sorted_versioned_subdirs(opt_root, "llvm@") {
            let bin = dir.join("bin").join(bare);
            if bin.is_file() {
                out.push(bin.to_string_lossy().into_owned());
            }
        }
    }

    out
}

fn sorted_versioned_subdirs(parent: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let Some(ver_str) = name.strip_prefix(prefix) else {
            continue;
        };
        let Some(parts) = parse_version_tuple(ver_str) else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            dirs.push((parts, path));
        }
    }
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    dirs.into_iter().map(|(_, p)| p).collect()
}

fn parse_version_tuple(s: &str) -> Option<Vec<u32>> {
    if s.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for seg in s.split('.') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(seg.parse::<u32>().ok()?);
    }
    Some(parts)
}

fn supports_riscv_target(kind: ToolKind, bin: &str, version_out: &str) -> bool {
    match kind {
        ToolKind::LlvmMc => has_riscv_target(version_out),
        ToolKind::Clang => Command::new(bin)
            .arg("-print-targets")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .is_some_and(|out| has_riscv_target(&combine_output(&out.stdout, &out.stderr))),
    }
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\')
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(stderr));
    }
    out
}

pub(crate) fn has_riscv_target(output: &str) -> bool {
    let Some(pos) = output.find("Registered Targets:") else {
        return false;
    };
    let after = &output[pos + "Registered Targets:".len()..];
    for line in after.lines() {
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        if matches!(first, "riscv32" | "riscv64") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn discover_candidates_order_for_explicit_path_and_standard_dirs() {
        let tmp = std::env::temp_dir().join(format!(
            "riscv_asm_lsp_tool_candidates_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let path_dir1 = tmp.join("p1");
        let path_dir2 = tmp.join("p2");
        let linux_lib = tmp.join("usr_lib");
        let brew_opt = tmp.join("brew_opt");
        let local_opt = tmp.join("local_opt");
        let win_bin = tmp.join("win_llvm_bin");

        let sub_bin = |root: &Path, subdir: &str, name: &str| {
            let p = root.join(subdir).join("bin").join(name);
            touch(&p);
            p.to_string_lossy().into_owned()
        };

        touch(&path_dir1.join("clang-18"));
        touch(&path_dir1.join("clang-foo"));
        touch(&path_dir2.join("clang-21"));
        touch(&path_dir2.join("clang-19"));
        let linux_18 = sub_bin(&linux_lib, "llvm-18", "clang");
        let linux_20 = sub_bin(&linux_lib, "llvm-20", "clang");
        let brew_llvm = sub_bin(&brew_opt, "llvm", "clang");
        let brew_19 = sub_bin(&brew_opt, "llvm@19", "clang");
        let brew_20 = sub_bin(&brew_opt, "llvm@20", "clang");
        let local_18 = sub_bin(&local_opt, "llvm@18", "clang");
        touch(&win_bin.join("llvm-mc.exe"));

        let roots = SystemRoots {
            linux_usr_lib: &linux_lib,
            homebrew_opt: &brew_opt,
            usr_local_opt: &local_opt,
            windows_llvm_bin: &win_bin,
        };
        let path_os = std::env::join_paths([&path_dir1, &path_dir2]).unwrap();

        assert_eq!(
            discover_candidates(
                ToolKind::Clang,
                Some(" /custom/clang-18 "),
                Some(&path_os),
                &roots,
                false
            ),
            vec!["/custom/clang-18"]
        );

        let expected = vec![
            "clang".to_owned(),
            "clang-21".to_owned(),
            "clang-19".to_owned(),
            "clang-18".to_owned(),
            linux_20,
            linux_18,
            brew_llvm,
            brew_20,
            brew_19,
            local_18,
        ];
        assert_eq!(
            discover_candidates(ToolKind::Clang, Some("  "), Some(&path_os), &roots, false),
            expected
        );
        assert_eq!(
            discover_candidates(ToolKind::Clang, None, Some(&path_os), &roots, false),
            expected
        );

        assert_eq!(
            discover_candidates(
                ToolKind::LlvmMc,
                None,
                Some(&OsString::from("")),
                &roots,
                true
            ),
            vec![
                "llvm-mc".to_owned(),
                win_bin.join("llvm-mc.exe").to_string_lossy().into_owned(),
            ]
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn has_riscv_target_checks_registered_targets_section() {
        let rv_targets =
            "\n  Registered Targets:\n    riscv32 - 32-bit RISC-V\n    x86-64 - 64-bit X86\n";
        assert!(has_riscv_target(rv_targets));

        let x86_only = "\n  Host CPU: riscv64\n  Registered Targets:\n    x86-64 - 64-bit X86\n";
        assert!(!has_riscv_target(x86_only));
        assert!(!has_riscv_target("no target table here"));
    }
}
