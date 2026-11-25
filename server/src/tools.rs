use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Clang,
    LlvmMc,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub struct ToolRunner {
    clang_bin: Mutex<String>,
    llvm_mc_bin: Mutex<String>,
    clang_disabled: AtomicBool,
    llvm_mc_disabled: AtomicBool,
    clang_logged: AtomicBool,
    llvm_mc_logged: AtomicBool,
    clang_timeout_logged: AtomicBool,
    llvm_mc_timeout_logged: AtomicBool,
    clang_version_checked: AtomicBool,
}

impl ToolRunner {
    pub fn new(clang_bin: String, llvm_mc_bin: String) -> Self {
        Self {
            clang_bin: Mutex::new(clang_bin),
            llvm_mc_bin: Mutex::new(llvm_mc_bin),
            clang_disabled: AtomicBool::new(false),
            llvm_mc_disabled: AtomicBool::new(false),
            clang_logged: AtomicBool::new(false),
            llvm_mc_logged: AtomicBool::new(false),
            clang_timeout_logged: AtomicBool::new(false),
            llvm_mc_timeout_logged: AtomicBool::new(false),
            clang_version_checked: AtomicBool::new(false),
        }
    }

    pub fn update_bins(&self, clang_bin: String, llvm_mc_bin: String) {
        if let Ok(mut g) = self.clang_bin.lock()
            && *g != clang_bin
        {
            *g = clang_bin;
            self.clang_disabled.store(false, Ordering::Relaxed);
            self.clang_logged.store(false, Ordering::Relaxed);
            self.clang_timeout_logged.store(false, Ordering::Relaxed);
            self.clang_version_checked.store(false, Ordering::Relaxed);
        }
        if let Ok(mut g) = self.llvm_mc_bin.lock()
            && *g != llvm_mc_bin
        {
            *g = llvm_mc_bin;
            self.llvm_mc_disabled.store(false, Ordering::Relaxed);
            self.llvm_mc_logged.store(false, Ordering::Relaxed);
            self.llvm_mc_timeout_logged.store(false, Ordering::Relaxed);
        }
    }

    pub fn run_clang(
        &self,
        args: &[String],
        stdin_text: &str,
        working_dir: Option<&Path>,
    ) -> Option<ToolOutput> {
        let bin = self
            .clang_bin
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "clang".to_owned());
        self.check_clang_version_once(&bin);
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
        let bin = self
            .llvm_mc_bin
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "llvm-mc".to_owned());
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
        match kind {
            ToolKind::Clang => self.clang_disabled.load(Ordering::Relaxed),
            ToolKind::LlvmMc => self.llvm_mc_disabled.load(Ordering::Relaxed),
        }
    }

    fn check_clang_version_once(&self, bin: &str) {
        if self.clang_version_checked.swap(true, Ordering::Relaxed) {
            return;
        }
        let Ok(out) = Command::new(bin).arg("--version").output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(major) = parse_clang_major_version(&stdout)
            && major != 19
        {
            eprintln!(
                "riscv-asm-lsp: warning: `{bin}` reports Clang major version {major}; expected LLVM/Clang 19 (diagnostics or extension behavior may differ)"
            );
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
        let logged = match kind {
            ToolKind::Clang => &self.clang_logged,
            ToolKind::LlvmMc => &self.llvm_mc_logged,
        };
        if !logged.swap(true, Ordering::Relaxed) {
            eprintln!("riscv-asm-lsp: {bin}: {reason}");
        }
    }

    fn log_timeout_once(&self, kind: ToolKind, bin: &str, timeout: Duration) {
        let (logged, feature) = match kind {
            ToolKind::Clang => (&self.clang_timeout_logged, "diagnostics"),
            ToolKind::LlvmMc => (&self.llvm_mc_timeout_logged, "hover expansion"),
        };
        if !logged.swap(true, Ordering::Relaxed) {
            eprintln!(
                "riscv-asm-lsp: `{bin}` timed out after {}s during {feature}",
                timeout.as_secs()
            );
        }
    }

    fn disable_once(&self, kind: ToolKind, bin: &str, reason: &str) {
        let (disabled, logged, feature) = match kind {
            ToolKind::Clang => (&self.clang_disabled, &self.clang_logged, "diagnostics"),
            ToolKind::LlvmMc => (
                &self.llvm_mc_disabled,
                &self.llvm_mc_logged,
                "hover expansions",
            ),
        };
        disabled.store(true, Ordering::Relaxed);
        if !logged.swap(true, Ordering::Relaxed) {
            eprintln!("riscv-asm-lsp: disabling {feature} ({bin}): {reason}");
        }
    }
}

fn parse_clang_major_version(output: &str) -> Option<u32> {
    let idx = output.find("version ")?;
    let after = &output[idx + "version ".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
