use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{MessageType, ShowMessageParams};

use crate::diagnostics::MessageSink;

const LLVM_INSTALL_URL: &str =
    "https://github.com/mihirokawai9472642/riscv-asm-lsp#installing-llvm-19";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Clang,
    LlvmMc,
}

impl ToolKind {
    fn default_name(self) -> &'static str {
        match self {
            Self::Clang => "clang",
            Self::LlvmMc => "llvm-mc",
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

        let mut chosen: Option<(String, String)> = None;
        let mut last_err: Option<String> = None;

        for cand in &candidates {
            if !is_explicit && looks_like_path(cand) && !Path::new(cand).is_file() {
                continue;
            }
            match Command::new(cand).arg("--version").output() {
                Ok(out) if out.status.success() => {
                    let combined = combine_output(&out.stdout, &out.stderr);
                    chosen = Some((cand.clone(), combined));
                    break;
                }
                Ok(out) => {
                    last_err = Some(format!("`{cand} --version` exited with {}", out.status));
                }
                Err(err) => {
                    last_err = Some(format!("failed to execute `{cand}`: {err}"));
                }
            }
        }

        let Some((bin, version_out)) = chosen else {
            slot.disabled = true;
            let requested = slot
                .explicit
                .as_deref()
                .unwrap_or_else(|| kind.default_name());
            let detail = last_err.unwrap_or_else(|| "executable not found".to_owned());
            let msg = format!(
                "RISC-V Assembly: {} disabled — `{requested}` not found ({detail}). Configure `lsp.riscv-asm-lsp.settings.{}` or install LLVM 19: {LLVM_INSTALL_URL}",
                kind.feature_name(),
                kind.setting_name(),
            );
            eprintln!(
                "riscv-asm-lsp: disabling {} ({requested}): {detail}",
                kind.feature_name()
            );
            self.notify_warning(&msg);
            return;
        };

        let major = parse_llvm_major_version(&version_out);
        let targets_ok = match kind {
            ToolKind::LlvmMc => has_riscv_target(&version_out),
            ToolKind::Clang => Command::new(&bin)
                .arg("-print-targets")
                .output()
                .ok()
                .filter(|out| out.status.success())
                .is_some_and(|out| has_riscv_target(&combine_output(&out.stdout, &out.stderr))),
        };

        if !targets_ok {
            slot.disabled = true;
            let ver_note = major
                .map(|m| format!(" (LLVM {m}; expected LLVM 19)"))
                .unwrap_or_default();
            let msg = format!(
                "RISC-V Assembly: {} disabled — `{bin}`{ver_note} does not include riscv32/riscv64 targets. Configure `lsp.riscv-asm-lsp.settings.{}` or install LLVM 19: {LLVM_INSTALL_URL}",
                kind.feature_name(),
                kind.setting_name(),
            );
            eprintln!(
                "riscv-asm-lsp: disabling {} ({bin}): missing riscv32/riscv64 target support{ver_note}",
                kind.feature_name()
            );
            self.notify_warning(&msg);
            return;
        }

        slot.resolved_bin = Some(bin.clone());
        if let Some(m) = major
            && m != 19
        {
            let msg = format!(
                "RISC-V Assembly: `{bin}` reports LLVM major version {m}; expected LLVM 19 (diagnostics or expansions may differ). Configure `lsp.riscv-asm-lsp.settings.{}` or see {LLVM_INSTALL_URL}",
                kind.setting_name(),
            );
            eprintln!(
                "riscv-asm-lsp: warning: `{bin}` reports LLVM major version {m}; expected LLVM 19"
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

pub fn candidate_bins(kind: ToolKind, explicit: Option<&str>) -> Vec<String> {
    candidate_bins_for_os(kind, explicit, cfg!(windows))
}

pub(crate) fn candidate_bins_for_os(
    kind: ToolKind,
    explicit: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return vec![p.to_owned()];
    }
    let (v19, bare, exe) = match kind {
        ToolKind::Clang => ("clang-19", "clang", "clang.exe"),
        ToolKind::LlvmMc => ("llvm-mc-19", "llvm-mc", "llvm-mc.exe"),
    };
    let mut out = vec![
        v19.to_owned(),
        bare.to_owned(),
        format!("/usr/lib/llvm-19/bin/{bare}"),
        format!("/opt/homebrew/opt/llvm@19/bin/{bare}"),
        format!("/usr/local/opt/llvm@19/bin/{bare}"),
    ];
    if is_windows {
        out.push(format!(r"C:\Program Files\LLVM\bin\{exe}"));
    }
    out
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

pub(crate) fn parse_llvm_major_version(output: &str) -> Option<u32> {
    let idx = output.find("version ")?;
    let after = &output[idx + "version ".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
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

    #[test]
    fn candidate_bins_order_for_explicit_and_auto() {
        assert_eq!(
            candidate_bins_for_os(ToolKind::Clang, Some(" /custom/clang-18 "), false),
            vec!["/custom/clang-18"]
        );
        assert_eq!(
            candidate_bins_for_os(ToolKind::Clang, Some("  "), false),
            candidate_bins_for_os(ToolKind::Clang, None, false)
        );
        assert_eq!(
            candidate_bins_for_os(ToolKind::Clang, None, false),
            vec![
                "clang-19",
                "clang",
                "/usr/lib/llvm-19/bin/clang",
                "/opt/homebrew/opt/llvm@19/bin/clang",
                "/usr/local/opt/llvm@19/bin/clang",
            ]
        );
        assert_eq!(
            candidate_bins_for_os(ToolKind::LlvmMc, None, true),
            vec![
                "llvm-mc-19",
                "llvm-mc",
                "/usr/lib/llvm-19/bin/llvm-mc",
                "/opt/homebrew/opt/llvm@19/bin/llvm-mc",
                "/usr/local/opt/llvm@19/bin/llvm-mc",
                r"C:\Program Files\LLVM\bin\llvm-mc.exe",
            ]
        );
    }

    #[test]
    fn parses_llvm_major_version_and_riscv_targets() {
        assert_eq!(
            parse_llvm_major_version("Debian clang version 19.1.7 (3+b1)"),
            Some(19)
        );
        assert_eq!(
            parse_llvm_major_version("Apple clang version 16.0.0 (clang-1600.0.26.3)"),
            Some(16)
        );
        assert_eq!(
            parse_llvm_major_version("Debian LLVM version 18.1.3\n  Optimized build."),
            Some(18)
        );

        let rv_targets =
            "\n  Registered Targets:\n    riscv32 - 32-bit RISC-V\n    x86-64 - 64-bit X86\n";
        assert!(has_riscv_target(rv_targets));

        let x86_only = "\n  Host CPU: riscv64\n  Registered Targets:\n    x86-64 - 64-bit X86\n";
        assert!(!has_riscv_target(x86_only));
        assert!(!has_riscv_target("no target table here"));
    }
}
