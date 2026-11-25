use std::fs;
use zed_extension_api::lsp::{Completion, CompletionKind};
use zed_extension_api::{
    self as zed, Architecture, CodeLabel, CodeLabelSpan, Command, DownloadedFileType,
    GithubReleaseOptions, LanguageServerId, LanguageServerInstallationStatus, Os, Result, Worktree,
};

const GITHUB_REPO: &str = "mihirokawai9472642/riscv-asm-lsp";

struct RiscvAssemblyExtension {
    cached_binary_path: Option<String>,
}

impl RiscvAssemblyExtension {
    fn language_server_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        if let Some(path) = worktree.which("riscv-asm-lsp") {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path
            && fs::metadata(path).is_ok_and(|stat| stat.is_file())
        {
            return Ok(path.clone());
        }

        let (platform, arch) = zed::current_platform();
        let (asset_name, file_type, exe_name) = target_asset_spec(platform, arch)?;

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = match zed::latest_github_release(
            GITHUB_REPO,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        ) {
            Ok(rel) => rel,
            Err(err) => {
                if let Some(fallback) = find_existing_downloaded_binary(exe_name) {
                    self.cached_binary_path = Some(fallback.clone());
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &LanguageServerInstallationStatus::None,
                    );
                    return Ok(fallback);
                }
                zed::set_language_server_installation_status(
                    language_server_id,
                    &LanguageServerInstallationStatus::Failed(err.clone()),
                );
                return Err(err);
            }
        };

        let version_dir = format!("riscv-asm-lsp-{}", release.version);
        let binary_path = format!("{version_dir}/{exe_name}");

        if !fs::metadata(&binary_path).is_ok_and(|stat| stat.is_file()) {
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == asset_name)
                .ok_or_else(|| {
                    let msg = format!(
                        "no release asset `{asset_name}` found in {}",
                        release.version
                    );
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &LanguageServerInstallationStatus::Failed(msg.clone()),
                    );
                    msg
                })?;

            zed::set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(&asset.download_url, &version_dir, file_type).inspect_err(
                |err| {
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &LanguageServerInstallationStatus::Failed(err.clone()),
                    );
                },
            )?;

            if platform != Os::Windows {
                zed::make_file_executable(&binary_path)?;
            }

            cleanup_old_versions(&version_dir);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

fn target_asset_spec(
    platform: Os,
    arch: Architecture,
) -> Result<(&'static str, DownloadedFileType, &'static str)> {
    match (platform, arch) {
        (Os::Linux, Architecture::X8664) => Ok((
            "riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz",
            DownloadedFileType::GzipTar,
            "riscv-asm-lsp",
        )),
        (Os::Linux, Architecture::Aarch64) => Ok((
            "riscv-asm-lsp-aarch64-unknown-linux-musl.tar.gz",
            DownloadedFileType::GzipTar,
            "riscv-asm-lsp",
        )),
        (Os::Mac, Architecture::X8664) => Ok((
            "riscv-asm-lsp-x86_64-apple-darwin.tar.gz",
            DownloadedFileType::GzipTar,
            "riscv-asm-lsp",
        )),
        (Os::Mac, Architecture::Aarch64) => Ok((
            "riscv-asm-lsp-aarch64-apple-darwin.tar.gz",
            DownloadedFileType::GzipTar,
            "riscv-asm-lsp",
        )),
        (Os::Windows, Architecture::X8664) => Ok((
            "riscv-asm-lsp-x86_64-pc-windows-msvc.zip",
            DownloadedFileType::Zip,
            "riscv-asm-lsp.exe",
        )),
        _ => Err(format!(
            "unsupported platform for riscv-asm-lsp prebuilt binary: {platform:?}/{arch:?}; install riscv-asm-lsp on PATH"
        )),
    }
}

fn find_existing_downloaded_binary(exe_name: &str) -> Option<String> {
    let entries = fs::read_dir(".").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("riscv-asm-lsp-") {
            let candidate = format!("{name_str}/{exe_name}");
            if fs::metadata(&candidate).is_ok_and(|s| s.is_file()) {
                return Some(candidate);
            }
        }
    }
    None
}

fn cleanup_old_versions(keep_dir: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("riscv-asm-lsp-") && name_str != keep_dir {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

impl zed::Extension for RiscvAssemblyExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let path = self.language_server_binary_path(language_server_id, worktree)?;
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
