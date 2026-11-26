use std::fs;
use zed_extension_api::lsp::{Completion, CompletionKind};
use zed_extension_api::serde_json::{self, json};
use zed_extension_api::settings::LspSettings;
use zed_extension_api::{
    self as zed, Architecture, CodeLabel, CodeLabelSpan, Command, DownloadedFileType,
    GithubReleaseOptions, LanguageServerId, LanguageServerInstallationStatus, Os, Result, Worktree,
};

const GITHUB_REPO: &str = "mihirokawai9472642/riscv-asm-lsp";
const INSTALL_DOCS_URL: &str =
    "https://github.com/mihirokawai9472642/riscv-asm-lsp#installing-a-release";

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

        // Track the newest non-prerelease release; server configuration keys remain backwards-compatible across 0.x releases.
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
                        "no release asset `{asset_name}` found in {} (see {INSTALL_DOCS_URL})",
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
            "unsupported platform for riscv-asm-lsp prebuilt binary: {platform:?}/{arch:?}; install riscv-asm-lsp on PATH ({INSTALL_DOCS_URL})"
        )),
    }
}

fn parse_version_tuple(dir_name: &str) -> Option<(u64, u64, u64)> {
    let raw = dir_name.strip_prefix("riscv-asm-lsp-")?;
    let trimmed = raw.trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

fn sort_cached_version_dirs(dirs: &mut [(String, String)]) {
    // Prefer the highest semantic version descending on offline fallback (so v0.10.0 > v0.9.0).
    dirs.sort_by(|a, b| {
        parse_version_tuple(&b.0)
            .cmp(&parse_version_tuple(&a.0))
            .then_with(|| b.0.cmp(&a.0))
    });
}

fn find_existing_downloaded_binary(exe_name: &str) -> Option<String> {
    let entries = fs::read_dir(".").ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        if name_str.starts_with("riscv-asm-lsp-") {
            let candidate = format!("{name_str}/{exe_name}");
            if fs::metadata(&candidate).is_ok_and(|s| s.is_file()) {
                candidates.push((name_str, candidate));
            }
        }
    }
    sort_cached_version_dirs(&mut candidates);
    candidates.into_iter().next().map(|(_, path)| path)
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
        if let Ok(lsp_settings) = LspSettings::for_worktree("riscv-asm-lsp", worktree)
            && let Some(binary) = lsp_settings.binary
            && let Some(path) = binary.path
        {
            return Ok(Command {
                command: path,
                args: binary.arguments.unwrap_or_default(),
                env: binary.env.unwrap_or_default().into_iter().collect(),
            });
        }

        let path = self.language_server_binary_path(language_server_id, worktree)?;
        Ok(Command {
            command: path,
            args: Vec::new(),
            env: Vec::new(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let options = LspSettings::for_worktree("riscv-asm-lsp", worktree)
            .ok()
            .and_then(|s| s.initialization_options);
        Ok(options)
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let settings = LspSettings::for_worktree("riscv-asm-lsp", worktree)
            .ok()
            .and_then(|s| s.settings);
        Ok(settings.map(|s| json!({ "riscv-asm-lsp": s })))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_dir_sorts_semver_numerically() {
        let mut dirs = vec![
            (
                "riscv-asm-lsp-v0.2.0".to_owned(),
                "riscv-asm-lsp-v0.2.0/riscv-asm-lsp".to_owned(),
            ),
            (
                "riscv-asm-lsp-v0.9.0".to_owned(),
                "riscv-asm-lsp-v0.9.0/riscv-asm-lsp".to_owned(),
            ),
            (
                "riscv-asm-lsp-v0.10.0".to_owned(),
                "riscv-asm-lsp-v0.10.0/riscv-asm-lsp".to_owned(),
            ),
            (
                "riscv-asm-lsp-0.11.1-rc1".to_owned(),
                "riscv-asm-lsp-0.11.1-rc1/riscv-asm-lsp".to_owned(),
            ),
            (
                "riscv-asm-lsp-custom".to_owned(),
                "riscv-asm-lsp-custom/riscv-asm-lsp".to_owned(),
            ),
        ];
        sort_cached_version_dirs(&mut dirs);
        let ordered: Vec<&str> = dirs.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(
            ordered,
            vec![
                "riscv-asm-lsp-0.11.1-rc1",
                "riscv-asm-lsp-v0.10.0",
                "riscv-asm-lsp-v0.9.0",
                "riscv-asm-lsp-v0.2.0",
                "riscv-asm-lsp-custom",
            ]
        );
    }
}
