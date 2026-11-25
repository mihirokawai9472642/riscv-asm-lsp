use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExternalDiagnosticsMode {
    #[default]
    Auto,
    On,
    Off,
}

impl ExternalDiagnosticsMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsConfig {
    pub enable: bool,
    pub delay_ms: u64,
    pub external: ExternalDiagnosticsMode,
    pub suppress: Vec<String>,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enable: true,
            delay_ms: 300,
            external: ExternalDiagnosticsMode::default(),
            suppress: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub clang: Option<String>,
    pub llvm_mc: Option<String>,
    pub compile_commands_dirs: Vec<String>,
    pub default_target: String,
    pub default_march: String,
    pub diagnostics: DiagnosticsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clang: None,
            llvm_mc: None,
            compile_commands_dirs: vec![String::new(), "build".to_owned()],
            default_target: "riscv64-unknown-elf".to_owned(),
            default_march: "rv64gc".to_owned(),
            diagnostics: DiagnosticsConfig::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagnosticsConfig {
    enable: Option<bool>,
    delay_ms: Option<u64>,
    external: Option<String>,
    suppress: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConfig {
    clang: Option<String>,
    llvm_mc: Option<String>,
    compile_commands_dirs: Option<Vec<String>>,
    default_target: Option<String>,
    default_march: Option<String>,
    diagnostics: Option<RawDiagnosticsConfig>,
}

impl Config {
    /// Parse server configuration from `initializationOptions`.
    /// Missing or invalid fields degrade safely to defaults.
    pub fn from_init_options(options: Option<Value>) -> Self {
        let Some(value) = options else {
            return Self::default();
        };
        if value.is_null() {
            return Self::default();
        }
        let raw: RawConfig = match serde_json::from_value(value) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("riscv-asm-lsp: invalid initializationOptions, using defaults: {err}");
                return Self::default();
            }
        };

        let mut cfg = Self::default();
        if let Some(clang) = raw
            .clang
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        {
            cfg.clang = Some(clang);
        }
        if let Some(llvm_mc) = raw
            .llvm_mc
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        {
            cfg.llvm_mc = Some(llvm_mc);
        }
        if let Some(dirs) = raw.compile_commands_dirs {
            cfg.compile_commands_dirs = dirs;
        }
        if let Some(target) = raw.default_target.filter(|s| !s.trim().is_empty()) {
            cfg.default_target = target;
        }
        if let Some(march) = raw.default_march.filter(|s| !s.trim().is_empty()) {
            cfg.default_march = march;
        }
        if let Some(diag) = raw.diagnostics {
            if let Some(enable) = diag.enable {
                cfg.diagnostics.enable = enable;
            }
            if let Some(delay_ms) = diag.delay_ms {
                cfg.diagnostics.delay_ms = delay_ms;
            }
            if let Some(ext_raw) = diag.external.as_deref()
                && let Some(mode) = ExternalDiagnosticsMode::parse(ext_raw)
            {
                cfg.diagnostics.external = mode;
            }
            if let Some(suppress) = diag.suppress {
                cfg.diagnostics.suppress = suppress
                    .into_iter()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_missing_or_invalid() {
        assert_eq!(Config::from_init_options(None), Config::default());
        assert_eq!(
            Config::from_init_options(Some(json!("not-an-object"))),
            Config::default()
        );
    }

    #[test]
    fn overrides_partial_fields() {
        let cfg = Config::from_init_options(Some(json!({
            "clang": "/usr/bin/clang-19",
            "defaultMarch": "rv32imac",
            "diagnostics": {
                "delayMs": 150
            }
        })));
        assert_eq!(cfg.clang.as_deref(), Some("/usr/bin/clang-19"));
        assert_eq!(cfg.llvm_mc, None);
        assert_eq!(cfg.default_march, "rv32imac");
        assert!(cfg.diagnostics.enable);
        assert_eq!(cfg.diagnostics.delay_ms, 150);
        assert_eq!(cfg.diagnostics.external, ExternalDiagnosticsMode::Auto);
        assert!(cfg.diagnostics.suppress.is_empty());
    }

    #[test]
    fn parses_external_diagnostics_mode_and_suppress() {
        for (raw, expected) in [
            ("auto", ExternalDiagnosticsMode::Auto),
            ("ON", ExternalDiagnosticsMode::On),
            ("off", ExternalDiagnosticsMode::Off),
            ("unknown", ExternalDiagnosticsMode::default()),
        ] {
            let cfg = Config::from_init_options(Some(json!({
                "diagnostics": {
                    "external": raw,
                    "suppress": ["gnu-only-directive", " "]
                }
            })));
            assert_eq!(cfg.diagnostics.external, expected, "raw={raw}");
            assert_eq!(cfg.diagnostics.suppress, vec!["gnu-only-directive"]);
        }
    }
}
