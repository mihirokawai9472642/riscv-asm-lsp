use std::io::{Read, Write};
use std::process::{Command, Stdio};
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
    clang_bin: String,
    llvm_mc_bin: String,
    clang_disabled: AtomicBool,
    llvm_mc_disabled: AtomicBool,
    clang_logged: AtomicBool,
    llvm_mc_logged: AtomicBool,
}

impl ToolRunner {
    pub fn new(clang_bin: String, llvm_mc_bin: String) -> Self {
        Self {
            clang_bin,
            llvm_mc_bin,
            clang_disabled: AtomicBool::new(false),
            llvm_mc_disabled: AtomicBool::new(false),
            clang_logged: AtomicBool::new(false),
            llvm_mc_logged: AtomicBool::new(false),
        }
    }

    pub fn run_clang(&self, args: &[String], stdin_text: &str) -> Option<ToolOutput> {
        self.run(
            ToolKind::Clang,
            &self.clang_bin,
            args,
            stdin_text,
            Duration::from_secs(5),
        )
    }

    pub fn run_llvm_mc(&self, args: &[String], stdin_text: &str) -> Option<ToolOutput> {
        self.run(
            ToolKind::LlvmMc,
            &self.llvm_mc_bin,
            args,
            stdin_text,
            Duration::from_secs(2),
        )
    }

    pub fn is_disabled(&self, kind: ToolKind) -> bool {
        match kind {
            ToolKind::Clang => self.clang_disabled.load(Ordering::Relaxed),
            ToolKind::LlvmMc => self.llvm_mc_disabled.load(Ordering::Relaxed),
        }
    }

    fn run(
        &self,
        kind: ToolKind,
        bin: &str,
        args: &[String],
        stdin_text: &str,
        timeout: Duration,
    ) -> Option<ToolOutput> {
        if self.is_disabled(kind) {
            return None;
        }

        let mut child = match Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(err) => {
                self.disable_once(kind, bin, &format!("failed to execute `{bin}`: {err}"));
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
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
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

        let status = status?;
        Some(ToolOutput {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        })
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
