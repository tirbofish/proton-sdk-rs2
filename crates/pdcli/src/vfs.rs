use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsSection {
    Root,
    MyFiles,
    Trash,
    Computers,
    Photos,
}

impl VfsSection {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Root => "/",
            Self::MyFiles => "MyFiles",
            Self::Trash => "Trash",
            Self::Computers => "Computers",
            Self::Photos => "Photos",
        }
    }
}

/// A location inside the virtual drive filesystem.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualPath {
    pub section: VfsSection,
    /// Path components within the section (does not include the section root itself).
    pub components: Vec<String>,
}

impl VirtualPath {
    pub fn root() -> Self {
        Self { section: VfsSection::Root, components: vec![] }
    }

    pub fn my_files() -> Self {
        Self { section: VfsSection::MyFiles, components: vec![] }
    }

    /// Resolves `input` relative to `base`, applying `..`, `~`, and absolute paths.
    pub fn resolve(base: &VirtualPath, input: &str) -> Self {
        let input = input.trim();

        if input == "~" || input == "/MyFiles" || input == "/MyFiles/" {
            return Self::my_files();
        }
        if input == "/" {
            return Self::root();
        }
        if input == "/Trash" || input == "/Trash/" {
            return Self { section: VfsSection::Trash, components: vec![] };
        }
        if input == "/Computers" || input == "/Computers/" {
            return Self { section: VfsSection::Computers, components: vec![] };
        }
        if input == "/Photos" || input == "/Photos/" {
            return Self { section: VfsSection::Photos, components: vec![] };
        }

        let (mut section, mut components) = if input.starts_with('/') {
            parse_absolute(input)
        } else if input.starts_with('~') {
            let rest = input.strip_prefix("~/").unwrap_or("");
            (VfsSection::MyFiles, split_components(rest))
        } else {
            (base.section.clone(), base.components.clone())
        };

        if !input.starts_with('/') && !input.starts_with('~') {
            components.extend(split_components(input));
        }

        // Resolve `..`
        let mut resolved: Vec<String> = Vec::new();
        for part in &components {
            match part.as_str() {
                ".." => {
                    if !resolved.pop().is_some() {
                        section = parent_section(&section);
                    }
                }
                "." | "" => {}
                _ => resolved.push(part.clone()),
            }
        }

        // When at root, a bare first component that matches a section name navigates into it.
        if section == VfsSection::Root && !resolved.is_empty() {
            match resolved[0].as_str() {
                "MyFiles" => { section = VfsSection::MyFiles; resolved.drain(..1); }
                "Trash" => { section = VfsSection::Trash; resolved.drain(..1); }
                "Computers" => { section = VfsSection::Computers; resolved.drain(..1); }
                "Photos" => { section = VfsSection::Photos; resolved.drain(..1); }
                _ => {}
            }
        }

        Self { section, components: resolved }
    }

    pub fn display(&self) -> String {
        match self.section {
            VfsSection::Root => {
                if self.components.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{}", self.components.join("/"))
                }
            }
            VfsSection::MyFiles => {
                if self.components.is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", self.components.join("/"))
                }
            }
            _ => {
                let base = format!("/{}", self.section.name());
                if self.components.is_empty() {
                    base
                } else {
                    format!("{}/{}", base, self.components.join("/"))
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn last_component(&self) -> Option<&str> {
        self.components.last().map(|s| s.as_str())
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display())
    }
}

fn split_components(s: &str) -> Vec<String> {
    s.split('/').filter(|p| !p.is_empty()).map(String::from).collect()
}

fn parse_absolute(input: &str) -> (VfsSection, Vec<String>) {
    let stripped = input.strip_prefix('/').unwrap_or(input);
    let mut parts = stripped.splitn(2, '/');
    let section_str = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    let section = match section_str {
        "MyFiles" | "" => VfsSection::MyFiles,
        "Trash" => VfsSection::Trash,
        "Computers" => VfsSection::Computers,
        "Photos" => VfsSection::Photos,
        _ => VfsSection::MyFiles,
    };
    (section, split_components(rest))
}

fn parent_section(section: &VfsSection) -> VfsSection {
    match section {
        VfsSection::Root => VfsSection::Root,
        _ => VfsSection::Root,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tilde() {
        let base = VirtualPath::my_files();
        assert_eq!(VirtualPath::resolve(&base, "~"), VirtualPath::my_files());
    }

    #[test]
    fn resolve_relative() {
        let base = VirtualPath { section: VfsSection::MyFiles, components: vec!["a".into()] };
        let result = VirtualPath::resolve(&base, "b");
        assert_eq!(result.components, vec!["a", "b"]);
    }

    #[test]
    fn resolve_dotdot() {
        let base = VirtualPath { section: VfsSection::MyFiles, components: vec!["a".into(), "b".into()] };
        let result = VirtualPath::resolve(&base, "..");
        assert_eq!(result.components, vec!["a"]);
    }

    #[test]
    fn resolve_absolute() {
        let base = VirtualPath::my_files();
        let result = VirtualPath::resolve(&base, "/Trash");
        assert_eq!(result.section, VfsSection::Trash);
        assert!(result.components.is_empty());
    }

    #[test]
    fn display_my_files_root() {
        assert_eq!(VirtualPath::my_files().display(), "~");
    }

    #[test]
    fn display_nested() {
        let p = VirtualPath {
            section: VfsSection::MyFiles,
            components: vec!["docs".into(), "reports".into()],
        };
        assert_eq!(p.display(), "~/docs/reports");
    }
}
