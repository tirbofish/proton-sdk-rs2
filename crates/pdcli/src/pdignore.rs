use std::path::PathBuf;

use globset::{Glob, GlobMatcher};
use platform_dirs::AppDirs;

const APP_NAME: &str = "pdcli";
const GLOBAL_FILE: &str = "pdignore";

pub const DEFAULT_GLOBAL_PDIGNORE: &str = r#"# pdcli global ignore
# Files matching these rules remain visible in the FUSE mount, but are not uploaded.

# OS metadata
.DS_Store
.Spotlight-V100/
.Trashes/
Thumbs.db
ehthumbs.db
Desktop.ini

# Editor swap and backup files
*~
*.swp
*.swo
*.tmp
*.temp
*.bak
*.orig
.#*
\#*#

# Office lock files
.~lock.*#
~$*

# pdcli folder ignore files
.pdignore

# Build/dependency folders
node_modules/
target/
dist/
build/
.gradle/
.idea/
.vscode/

# VCS internals
.git/
.hg/
.svn/

# Python/cache noise
__pycache__/
*.py[cod]
.pytest_cache/
.mypy_cache/
.ruff_cache/

# Logs and local env files
*.log
.env
.env.*
"#;

pub fn global_path() -> PathBuf {
    AppDirs::new(Some(APP_NAME), false)
        .expect("failed to resolve platform config directory")
        .config_dir
        .join(GLOBAL_FILE)
}

pub fn load_global_text() -> String {
    let path = global_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            let _ = save_global_text(DEFAULT_GLOBAL_PDIGNORE);
            DEFAULT_GLOBAL_PDIGNORE.to_string()
        }
    }
}

pub fn save_global_text(text: &str) -> anyhow::Result<()> {
    let path = global_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

struct Rule {
    negated: bool,
    dir_only: bool,
    matchers: Vec<GlobMatcher>,
}

pub struct IgnoreMatcher {
    rules: Vec<Rule>,
}

impl IgnoreMatcher {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_ignore_text(&mut self, text: &str) {
        for line in text.lines() {
            if let Some(rule) = parse_rule(line) {
                self.rules.push(rule);
            }
        }
    }

    pub fn check(&self, relative_path: &str, is_dir: bool) -> Option<bool> {
        let relative_path = normalize_path(relative_path);
        let mut ignored = None;
        for rule in &self.rules {
            if rule.matches(&relative_path, is_dir) {
                ignored = Some(!rule.negated);
            }
        }
        ignored
    }
}

impl Rule {
    fn matches(&self, relative_path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir && !self.matches_parent_dir(relative_path) {
            return false;
        }

        self.matchers
            .iter()
            .any(|matcher| matcher.is_match(relative_path))
    }

    fn matches_parent_dir(&self, relative_path: &str) -> bool {
        let mut parent = normalize_path(relative_path);
        while let Some((next_parent, _)) = parent.rsplit_once('/') {
            if next_parent.is_empty() {
                break;
            }
            parent = next_parent.to_string();
            if self
                .matchers
                .iter()
                .any(|matcher| matcher.is_match(parent.as_str()))
            {
                return true;
            }
        }
        false
    }
}

fn parse_rule(line: &str) -> Option<Rule> {
    let mut pattern = line.trim_end();
    if pattern.is_empty() {
        return None;
    }
    if pattern.starts_with('#') {
        return None;
    }

    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    if let Some(rest) = pattern.strip_prefix("\\#") {
        pattern = rest;
    }

    let anchored = pattern.starts_with('/');
    let dir_only = pattern.ends_with('/');
    let pattern = pattern.trim_matches('/');
    if pattern.is_empty() {
        return None;
    }

    let has_slash = pattern.contains('/');
    let mut patterns = Vec::new();

    if anchored || has_slash {
        patterns.push(pattern.to_string());
        if !anchored {
            patterns.push(format!("**/{pattern}"));
        }
    } else {
        patterns.push(pattern.to_string());
        patterns.push(format!("**/{pattern}"));
    }

    if dir_only {
        let children = patterns
            .iter()
            .map(|pattern| format!("{pattern}/**"))
            .collect::<Vec<_>>();
        patterns.extend(children);
    }

    let matchers = patterns
        .into_iter()
        .filter_map(|pattern| Glob::new(&pattern).ok())
        .map(|glob| glob.compile_matcher())
        .collect::<Vec<_>>();

    if matchers.is_empty() {
        None
    } else {
        Some(Rule {
            negated,
            dir_only,
            matchers,
        })
    }
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::IgnoreMatcher;

    fn matcher(text: &str) -> IgnoreMatcher {
        let mut matcher = IgnoreMatcher::new();
        matcher.add_ignore_text(text);
        matcher
    }

    #[test]
    fn matches_common_gitignore_patterns() {
        let matcher = matcher("*.log\nbuild/\n/docs/*.tmp\n");

        assert_eq!(matcher.check("app.log", false), Some(true));
        assert_eq!(matcher.check("nested/app.log", false), Some(true));
        assert_eq!(matcher.check("build", true), Some(true));
        assert_eq!(matcher.check("build/output.o", false), Some(true));
        assert_eq!(matcher.check("src/build/output.o", false), Some(true));
        assert_eq!(matcher.check("docs/file.tmp", false), Some(true));
        assert_eq!(matcher.check("src/docs/file.tmp", false), None);
    }

    #[test]
    fn later_negations_override_earlier_rules() {
        let matcher = matcher("*.tmp\n!important.tmp\n");

        assert_eq!(matcher.check("scratch.tmp", false), Some(true));
        assert_eq!(matcher.check("important.tmp", false), Some(false));
    }
}
