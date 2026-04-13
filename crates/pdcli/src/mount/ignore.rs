use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::thumbnail::is_lock_file;

/// The .pdclignore filename constant.
pub(super) const PDCLIGNORE_FILENAME: &str = ".pdclignore";

/// Default patterns for the global .pdclignore file.
const DEFAULT_PDCLIGNORE_PATTERNS: &str = r#"# pdcli ignore file
# Lines starting with # are comments
# Patterns use gitignore-style glob matching

# Common temporary files
*.tmp
*.temp
*.swp
*.swo
*~
.goutputstream-*

# OS-generated files
.DS_Store
.DS_Store?
._*
.Spotlight-V100
.Trashes
Thumbs.db
ehthumbs.db
Desktop.ini

# Trash directories (Linux/freedesktop.org)
.Trash
.Trash-*

# Partial downloads
*.part
*.partial
*.download
*.crdownload

# Editor backup files
*~
*.bak
*.backup
\#*\#
.#*

# LibreOffice/OpenOffice lock files
.~lock.*#

# Log files
*.log

# Cache directories and files
.cache/
__pycache__/
*.pyc
.pytest_cache/
node_modules/
.npm/
.yarn/

# Build artifacts
*.o
*.obj
*.a
*.lib
*.so
*.dll
*.dylib

# IDE and editor directories
.idea/
.vscode/
*.xcworkspace/
*.xcodeproj/
"#;

/// Manager for .pdclignore patterns.
/// Supports a global config file and per-directory .pdclignore files.
pub(super) struct IgnoreManager {
    /// Path to the global .pdclignore file in config directory.
    global_ignore_path: PathBuf,
    /// Compiled global patterns (from config directory).
    global_patterns: GlobSet,
    /// Cached per-directory patterns: directory path -> compiled patterns.
    #[allow(dead_code)]
    directory_patterns: BTreeMap<u64, GlobSet>,
}

impl IgnoreManager {
    /// Create a new IgnoreManager, loading the global .pdclignore from config.
    pub(super) fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("pdcli");

        std::fs::create_dir_all(&config_dir)
            .context("Failed to create config directory")?;

        let global_ignore_path = config_dir.join(PDCLIGNORE_FILENAME);

        if !global_ignore_path.exists() {
            std::fs::write(&global_ignore_path, DEFAULT_PDCLIGNORE_PATTERNS)
                .context("Failed to write default .pdclignore file")?;
            tracing::info!("Created default .pdclignore at {:?}", global_ignore_path);
        }

        let global_patterns = Self::load_patterns_from_file(&global_ignore_path)?;

        Ok(Self {
            global_ignore_path,
            global_patterns,
            directory_patterns: BTreeMap::new(),
        })
    }

    /// Load and compile patterns from a .pdclignore file.
    fn load_patterns_from_file(path: &Path) -> Result<GlobSet> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read ignore file: {:?}", path))?;
        Self::compile_patterns(&content)
    }

    /// Compile patterns from a string (file content).
    pub(super) fn compile_patterns(content: &str) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();

        for line in content.lines() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if line.starts_with('!') {
                tracing::debug!("Negation patterns not supported, skipping: {}", line);
                continue;
            }

            let patterns_to_add = if line.ends_with('/') {
                let without_slash = line.trim_end_matches('/');
                vec![line.to_string(), without_slash.to_string()]
            } else {
                vec![line.to_string()]
            };

            for pattern in patterns_to_add {
                match Glob::new(&pattern) {
                    Ok(glob) => {
                        builder.add(glob);
                    }
                    Err(e) => {
                        tracing::warn!("Invalid glob pattern '{}': {}", pattern, e);
                    }
                }
            }
        }

        builder.build().context("Failed to build glob set")
    }

    /// Check if a filename should be ignored based on global patterns.
    pub(super) fn is_ignored_global(&self, filename: &str) -> bool {
        if filename == PDCLIGNORE_FILENAME {
            return false;
        }

        if is_lock_file(filename) {
            return true;
        }

        self.global_patterns.is_match(filename)
    }

    /// Check if a filename should be ignored based on local .pdclignore content.
    #[allow(dead_code)]
    pub(super) fn is_ignored_by_content(&self, filename: &str, local_ignore_content: Option<&str>) -> bool {
        if filename == PDCLIGNORE_FILENAME {
            return false;
        }

        if self.global_patterns.is_match(filename) {
            return true;
        }

        if let Some(content) = local_ignore_content {
            if let Ok(local_patterns) = Self::compile_patterns(content) {
                if local_patterns.is_match(filename) {
                    return true;
                }
            }
        }

        false
    }

    /// Reload global patterns from the config file.
    #[allow(dead_code)]
    pub(super) fn reload_global(&mut self) -> Result<()> {
        self.global_patterns = Self::load_patterns_from_file(&self.global_ignore_path)?;
        tracing::info!("Reloaded global .pdclignore patterns");
        Ok(())
    }

    /// Get the path to the global .pdclignore file.
    pub(super) fn global_ignore_path(&self) -> &Path {
        &self.global_ignore_path
    }
}
