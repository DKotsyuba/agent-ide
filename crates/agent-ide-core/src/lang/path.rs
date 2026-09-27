//! Symbol paths: `src/index.ts#ClassImpl/method`.
//!
//! `#` separates the file from the symbol so a file name may contain anything; `/` inside the
//! symbol part is nesting (type, impl, class, namespace, module → member). A path without a file
//! part (`#BindingStatus` or `BindingStatus`) is a project-wide name lookup.

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// Parsed symbol address; `file` is relative to the project root.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct SymbolPath {
    file: Option<PathBuf>,
    segments: Vec<String>,
}

/// Why a path string was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    Empty,
    EmptySegment,
    AbsoluteFile,
    ParentSegment,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty symbol path",
            Self::EmptySegment => "empty symbol segment",
            Self::AbsoluteFile => "symbol file must be relative to the project root",
            Self::ParentSegment => "symbol file must not contain `..`",
        })
    }
}

impl std::error::Error for PathError {}

impl SymbolPath {
    /// Parses `file#A/b`, `#A/b` or `A/b`.
    pub fn parse(text: &str) -> Result<Self, PathError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(PathError::Empty);
        }
        let (file, symbol) = match text.split_once('#') {
            Some((file, symbol)) => (Some(file), symbol),
            None => (None, text),
        };
        let file = match file {
            Some("") | None => None,
            Some(file) => {
                let file = Path::new(file);
                if file.is_absolute() {
                    return Err(PathError::AbsoluteFile);
                }
                if file
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    return Err(PathError::ParentSegment);
                }
                Some(file.to_path_buf())
            }
        };
        let segments = if symbol.is_empty() {
            Vec::new()
        } else {
            symbol
                .split('/')
                .map(|segment| {
                    let segment = segment.trim();
                    if segment.is_empty() {
                        Err(PathError::EmptySegment)
                    } else {
                        Ok(segment.to_owned())
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        if file.is_none() && segments.is_empty() {
            return Err(PathError::Empty);
        }
        Ok(Self { file, segments })
    }

    /// Builds a path from parts already known to be valid.
    pub fn new(file: Option<PathBuf>, segments: Vec<String>) -> Self {
        Self { file, segments }
    }

    /// Relative file, absent for a project-wide name lookup.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Nesting segments, outermost first; empty when the path names a whole file.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Last segment: the symbol's own name.
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// Path of the owner (all segments but the last), if any.
    pub fn owner(&self) -> Option<SymbolPath> {
        if self.segments.len() < 2 {
            return None;
        }
        Some(Self {
            file: self.file.clone(),
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// Path of a member of this symbol.
    pub fn child(&self, name: &str) -> SymbolPath {
        let mut segments = self.segments.clone();
        segments.push(name.to_owned());
        Self {
            file: self.file.clone(),
            segments,
        }
    }

    /// Whether the path names a whole file rather than a symbol.
    pub fn is_file(&self) -> bool {
        self.segments.is_empty()
    }
}

impl fmt::Display for SymbolPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}", file.display())?;
        }
        f.write_str("#")?;
        f.write_str(&self.segments.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_owner_and_name() {
        let path = SymbolPath::parse("src/a.rs#Guard/new").unwrap();
        assert_eq!(path.file(), Some(Path::new("src/a.rs")));
        assert_eq!(path.segments(), ["Guard", "new"]);
        assert_eq!(path.name(), Some("new"));
        assert_eq!(path.owner().unwrap().to_string(), "src/a.rs#Guard");
        assert_eq!(path.to_string(), "src/a.rs#Guard/new");
        assert_eq!(path.child("x").to_string(), "src/a.rs#Guard/new/x");
    }

    #[test]
    fn name_only_and_file_only_forms() {
        let name = SymbolPath::parse("BindingStatus").unwrap();
        assert_eq!(name.file(), None);
        assert_eq!(name.to_string(), "#BindingStatus");
        assert_eq!(SymbolPath::parse("#BindingStatus").unwrap(), name);
        let file = SymbolPath::parse("src/a.rs#").unwrap();
        assert!(file.is_file());
        assert_eq!(file.to_string(), "src/a.rs#");
        // Without `#` the whole text is a nested name lookup, never a file.
        assert_eq!(SymbolPath::parse("src/a.rs").unwrap().file(), None);
    }

    #[test]
    fn rejects_bad_shapes() {
        assert_eq!(SymbolPath::parse("").unwrap_err(), PathError::Empty);
        assert_eq!(SymbolPath::parse("#").unwrap_err(), PathError::Empty);
        assert_eq!(
            SymbolPath::parse("a.rs#A//b").unwrap_err(),
            PathError::EmptySegment
        );
        assert_eq!(
            SymbolPath::parse("/etc/x#A").unwrap_err(),
            PathError::AbsoluteFile
        );
        assert_eq!(
            SymbolPath::parse("../x.rs#A").unwrap_err(),
            PathError::ParentSegment
        );
    }
}
