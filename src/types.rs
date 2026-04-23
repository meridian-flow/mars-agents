use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Serialize, Deserialize, Hash, Eq, PartialEq, Clone, Debug, Ord, PartialOrd,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

string_newtype!(SourceName);
string_newtype!(ItemName);
string_newtype!(SourceUrl);
string_newtype!(CommitHash);
string_newtype!(ContentHash);

/// Normalized relative package coordinate under a fetched source root.
#[derive(Hash, Eq, PartialEq, Clone, Debug, Ord, PartialOrd)]
pub struct SourceSubpath(String);

impl SourceSubpath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, SourceSubpathError> {
        let raw = value.as_ref();
        if raw.is_empty() {
            return Err(SourceSubpathError::Empty);
        }

        let normalized_separators = raw.replace('\\', "/");
        if is_windows_absolute(&normalized_separators) {
            return Err(SourceSubpathError::Absolute {
                input: raw.to_string(),
            });
        }

        let mut segments = Vec::new();
        for component in Path::new(&normalized_separators).components() {
            match component {
                Component::Normal(seg) => segments.push(seg.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(SourceSubpathError::Escaping {
                        input: raw.to_string(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(SourceSubpathError::Absolute {
                        input: raw.to_string(),
                    });
                }
            }
        }

        if segments.is_empty() {
            return Err(SourceSubpathError::Empty);
        }

        Ok(Self(segments.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// Join this relative subpath under `base`, rejecting traversal attempts.
    pub fn join_under(&self, base: &Path) -> Result<PathBuf, SourceSubpathError> {
        let mut joined = base.to_path_buf();
        for component in self.as_path().components() {
            match component {
                Component::Normal(seg) => joined.push(seg),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(SourceSubpathError::Escaping {
                        input: self.0.clone(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(SourceSubpathError::Absolute {
                        input: self.0.clone(),
                    });
                }
            }
        }

        if joined.strip_prefix(base).is_err() {
            return Err(SourceSubpathError::Escaping {
                input: self.0.clone(),
            });
        }

        Ok(joined)
    }
}

impl fmt::Display for SourceSubpath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SourceSubpath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::str::FromStr for SourceSubpath {
    type Err = SourceSubpathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for SourceSubpath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SourceSubpath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        SourceSubpath::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceSubpathError {
    #[error("subpath cannot be empty")]
    Empty,
    #[error("subpath must be relative, got absolute value: {input:?}")]
    Absolute { input: String },
    #[error("subpath cannot escape package root: {input:?}")]
    Escaping { input: String },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DestPathError {
    #[error("destination path cannot be empty")]
    Empty,
    #[error("destination path must be relative, got absolute value: {input:?}")]
    Absolute { input: String },
    #[error("destination path cannot escape target root: {input:?}")]
    Escaping { input: String },
    #[error("cannot convert path to DestPath: {reason}")]
    ConversionFailed { reason: String },
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if path.starts_with('/') {
        return true;
    }
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return true;
    }
    false
}

fn is_windows_drive_relative(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Where an item came from — used for lock provenance and display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceOrigin {
    /// From a dependency (git or path source).
    Dependency(SourceName),
    /// From the local project's [package] declaration.
    LocalPackage,
}

impl fmt::Display for SourceOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dependency(name) => write!(f, "{name}"),
            Self::LocalPackage => write!(f, "_self"),
        }
    }
}

/// Kind of installable item.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Agent,
    Skill,
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemKind::Agent => write!(f, "agent"),
            ItemKind::Skill => write!(f, "skill"),
        }
    }
}

/// Stable identity for an installed item — decoupled from source URL.
///
/// Items are identified by `(kind, name)`, not by source URL.
/// If a package moves to a different git host, the item identity is preserved.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId {
    pub kind: ItemKind,
    pub name: ItemName,
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind, self.name)
    }
}

/// Normalized relative path coordinate (always forward-slash).
/// Use `resolve(root)` to get a native filesystem path.
#[derive(Eq, PartialEq, Clone, Debug, Ord, PartialOrd)]
pub struct DestPath(String);

impl DestPath {
    /// Create from any string, normalizing separators and rejecting invalid paths.
    pub fn new(value: impl AsRef<str>) -> Result<Self, DestPathError> {
        let raw = value.as_ref();
        if raw.is_empty() {
            return Err(DestPathError::Empty);
        }

        let normalized_separators = raw.replace('\\', "/");
        if is_windows_absolute(&normalized_separators)
            || is_windows_drive_relative(&normalized_separators)
        {
            return Err(DestPathError::Absolute {
                input: raw.to_string(),
            });
        }

        let mut segments = Vec::new();
        for component in Path::new(&normalized_separators).components() {
            match component {
                Component::Normal(seg) => segments.push(seg.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(DestPathError::Escaping {
                        input: raw.to_string(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(DestPathError::Absolute {
                        input: raw.to_string(),
                    });
                }
            }
        }

        if segments.is_empty() {
            return Err(DestPathError::Empty);
        }

        Ok(Self(segments.join("/")))
    }

    /// The normalized string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume and return the inner string.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Resolve to a native filesystem path under the given root.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        let mut result = root.to_path_buf();
        for component in self.components() {
            result.push(component);
        }
        result
    }

    /// Split into path components (by forward slash).
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Create from a host-relative path by stripping a root prefix.
    /// Used for CLI commands that accept filesystem paths.
    pub fn from_host_relative(path: &Path, root: &Path) -> Result<Self, DestPathError> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| DestPathError::ConversionFailed {
                reason: format!("path {:?} is not under root {:?}", path, root),
            })?;

        let mut segments = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(seg) => segments.push(seg.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(DestPathError::Escaping {
                        input: path.to_string_lossy().into_owned(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(DestPathError::Absolute {
                        input: path.to_string_lossy().into_owned(),
                    });
                }
            }
        }

        if segments.is_empty() {
            return Err(DestPathError::Empty);
        }

        Self::new(segments.join("/"))
    }
}

impl From<&str> for DestPath {
    fn from(value: &str) -> Self {
        Self::new(value).expect("invalid destination path")
    }
}

impl From<String> for DestPath {
    fn from(value: String) -> Self {
        Self::new(value).expect("invalid destination path")
    }
}

impl AsRef<str> for DestPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for DestPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Hash for DestPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for DestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for DestPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DestPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        DestPath::new(value).map_err(serde::de::Error::custom)
    }
}

/// Resolved context for a mars command — project root + managed output root.
///
/// Named fields prevent argument-order bugs that plague `(project_root, managed_root)` pairs.
#[derive(Debug, Clone)]
pub struct MarsContext {
    /// Project root containing mars.toml and mars.lock.
    pub project_root: PathBuf,
    /// Managed output directory (e.g. /project/.agents).
    pub managed_root: PathBuf,
}

#[cfg(test)]
impl MarsContext {
    /// Create a MarsContext for tests without any validation.
    pub fn for_test(project_root: PathBuf, managed_root: PathBuf) -> Self {
        MarsContext {
            project_root,
            managed_root,
        }
    }
}

/// Stable source identity used for resolver deduplication.
#[derive(Hash, Eq, PartialEq, Clone, Debug, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SourceId {
    Git {
        url: SourceUrl,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<SourceSubpath>,
    },
    Path {
        canonical: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<SourceSubpath>,
    },
}

impl SourceId {
    pub fn git(url: SourceUrl) -> Self {
        Self::Git { url, subpath: None }
    }

    pub fn git_with_subpath(url: SourceUrl, subpath: Option<SourceSubpath>) -> Self {
        Self::Git { url, subpath }
    }

    pub fn path(base: &Path, relative_or_absolute: &Path) -> std::io::Result<Self> {
        Self::path_with_subpath(base, relative_or_absolute, None)
    }

    pub fn path_with_subpath(
        base: &Path,
        relative_or_absolute: &Path,
        subpath: Option<SourceSubpath>,
    ) -> std::io::Result<Self> {
        let candidate = if relative_or_absolute.is_absolute() {
            relative_or_absolute.to_path_buf()
        } else {
            base.join(relative_or_absolute)
        };
        let canonical = candidate.canonicalize()?;
        Ok(Self::Path { canonical, subpath })
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git { url, subpath } => {
                write!(f, "git:{url}")?;
                if let Some(subpath) = subpath {
                    write!(f, "@{subpath}")?;
                }
                Ok(())
            }
            Self::Path { canonical, subpath } => {
                write!(f, "path:{}", canonical.display())?;
                if let Some(subpath) = subpath {
                    write!(f, "@{subpath}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameRule {
    pub from: ItemName,
    pub to: ItemName,
}

/// Ordered rename rules, serialized as TOML inline table/map for compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameMap(Vec<RenameRule>);

impl RenameMap {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn insert(&mut self, from: ItemName, to: ItemName) {
        if let Some(existing) = self.0.iter_mut().find(|r| r.from == from) {
            existing.to = to;
            return;
        }
        self.0.push(RenameRule { from, to });
    }

    pub fn push(&mut self, rule: RenameRule) {
        self.insert(rule.from, rule.to);
    }

    pub fn get(&self, from: &str) -> Option<&ItemName> {
        self.0.iter().find(|r| r.from == from).map(|r| &r.to)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RenameRule> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Serialize for RenameMap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for rule in &self.0 {
            map.serialize_entry(rule.from.as_str(), rule.to.as_str())?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for RenameMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = indexmap::IndexMap::<String, String>::deserialize(deserializer)?;
        Ok(Self(
            map.into_iter()
                .map(|(from, to)| RenameRule {
                    from: ItemName::from(from),
                    to: ItemName::from(to),
                })
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Wrapper<T> {
        value: T,
    }

    #[test]
    fn dest_path_roundtrip() {
        let v = Wrapper {
            value: DestPath::from("agents/coder.md"),
        };
        let s = toml::to_string(&v).unwrap();
        let out: Wrapper<DestPath> = toml::from_str(&s).unwrap();
        assert_eq!(v, out);
    }

    #[test]
    fn rename_map_toml_roundtrip_compat() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct RenameWrapper {
            rename: RenameMap,
        }

        let input = r#"rename = { "coder" = "cool-coder" }"#;
        let parsed: RenameWrapper = toml::from_str(input).unwrap();
        assert_eq!(
            parsed.rename.get("coder").map(|v| v.as_str()),
            Some("cool-coder")
        );

        let serialized = toml::to_string(&parsed).unwrap();
        let reparsed: RenameWrapper = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn source_subpath_normalizes_windows_and_unix_separators() {
        let subpath = SourceSubpath::new(r"plugins\foo/bar\baz").unwrap();
        assert_eq!(subpath.as_str(), "plugins/foo/bar/baz");
    }

    #[test]
    fn source_subpath_rejects_empty() {
        let err = SourceSubpath::new("").unwrap_err();
        assert_eq!(err, SourceSubpathError::Empty);
    }

    #[test]
    fn source_subpath_rejects_absolute() {
        let err = SourceSubpath::new("/abs/path").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Absolute { .. }));
    }

    #[test]
    fn source_subpath_rejects_root_only() {
        let err = SourceSubpath::new("/").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Absolute { .. }));
    }

    #[test]
    fn source_subpath_rejects_windows_absolute() {
        let err = SourceSubpath::new(r"C:\abs\path").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Absolute { .. }));
    }

    #[test]
    fn source_subpath_rejects_escape() {
        let err = SourceSubpath::new("../escape").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Escaping { .. }));
    }

    #[test]
    fn source_subpath_accepts_nested_relative_path() {
        let subpath = SourceSubpath::new("a/b/c").unwrap();
        assert_eq!(subpath.as_str(), "a/b/c");
    }

    #[test]
    fn source_subpath_accepts_plugins_foo() {
        let subpath = SourceSubpath::new("plugins/foo").unwrap();
        assert_eq!(subpath.as_str(), "plugins/foo");
    }

    #[test]
    fn source_subpath_serializes_with_forward_slashes() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct SubpathWrapper {
            subpath: SourceSubpath,
        }

        let wrapper = SubpathWrapper {
            subpath: SourceSubpath::new(r"plugins\foo").unwrap(),
        };
        let toml = toml::to_string(&wrapper).unwrap();
        assert!(toml.contains("subpath = \"plugins/foo\""));
    }

    #[test]
    fn source_subpath_join_under_base() {
        let base = PathBuf::from("/tmp/mars");
        let subpath = SourceSubpath::new("plugins/foo").unwrap();
        let joined = subpath.join_under(&base).unwrap();
        assert_eq!(joined, base.join("plugins").join("foo"));
    }

    #[test]
    fn source_subpath_join_under_rejects_escape_path() {
        let escaped = SourceSubpath(String::from("../escape"));
        let err = escaped.join_under(Path::new("/tmp/base")).unwrap_err();
        assert!(matches!(err, SourceSubpathError::Escaping { .. }));
    }

    // --- Additional edge cases ---

    // Edge case 4: deeply nested path (5 levels)
    #[test]
    fn source_subpath_accepts_deeply_nested() {
        let subpath = SourceSubpath::new("a/b/c/d/e").unwrap();
        assert_eq!(subpath.as_str(), "a/b/c/d/e");
    }

    // Edge case 7: Windows drive letter with forward slash (C:/foo)
    #[test]
    fn source_subpath_rejects_windows_drive_forward_slash() {
        let err = SourceSubpath::new("C:/foo").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Absolute { .. }));
    }

    // Edge case 9: "." alone — CurDir is skipped → segments empty → Empty error
    #[test]
    fn source_subpath_rejects_current_dir_dot() {
        let err = SourceSubpath::new(".").unwrap_err();
        assert_eq!(err, SourceSubpathError::Empty);
    }

    // Edge case 11: mid-path parent escape "a/../../escape" — hits ParentDir immediately after
    // pushing "a", so it is rejected as Escaping (conservative: any ".." rejected)
    #[test]
    fn source_subpath_rejects_mid_path_double_parent_escape() {
        let err = SourceSubpath::new("a/../../escape").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Escaping { .. }));
    }

    // Edge case 12: "a/b/../c" — conservative policy: any ".." is rejected as Escaping,
    // even when logically harmless. This documents and pins the chosen policy.
    #[test]
    fn source_subpath_rejects_harmless_parent_in_middle() {
        let err = SourceSubpath::new("a/b/../c").unwrap_err();
        assert!(matches!(err, SourceSubpathError::Escaping { .. }));
    }

    // Edge case 13: trailing slash normalizes (no trailing slash in canonical form)
    #[test]
    fn source_subpath_normalizes_trailing_slash() {
        let subpath = SourceSubpath::new("plugins/foo/").unwrap();
        assert_eq!(subpath.as_str(), "plugins/foo");
    }

    // Edge case 14: leading "./" normalizes to the bare path
    #[test]
    fn source_subpath_normalizes_leading_dot_slash() {
        let subpath = SourceSubpath::new("./plugins/foo").unwrap();
        assert_eq!(subpath.as_str(), "plugins/foo");
    }

    // join_under: base path with trailing slash (PathBuf handles it consistently)
    #[test]
    fn source_subpath_join_under_base_with_trailing_slash() {
        let base = PathBuf::from("/tmp/mars/");
        let subpath = SourceSubpath::new("plugins/foo").unwrap();
        let joined = subpath.join_under(&base).unwrap();
        // PathBuf normalizes trailing slash — result should be /tmp/mars/plugins/foo
        assert_eq!(joined, PathBuf::from("/tmp/mars/plugins/foo"));
    }

    // JSON serde round-trip: LockedSource without subpath → subpath = None
    #[test]
    fn locked_source_json_roundtrip_without_subpath() {
        let json = r#"{"url":"https://github.com/org/base.git"}"#;
        let parsed: crate::lock::LockedSource = serde_json::from_str(json).unwrap();
        assert!(parsed.subpath.is_none());
    }

    // JSON serde round-trip: LockedSource with subpath serializes as forward-slash string
    #[test]
    fn locked_source_json_roundtrip_with_subpath() {
        let source = crate::lock::LockedSource {
            url: Some(SourceUrl::from("https://github.com/org/base.git")),
            path: None,
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
            version: None,
            commit: None,
            tree_hash: None,
        };
        let json = serde_json::to_string(&source).unwrap();
        assert!(json.contains("\"subpath\":\"plugins/foo\""));
        let reparsed: crate::lock::LockedSource = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reparsed.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    // Backward compat: old lock TOML with no subpath field deserializes with subpath = None (RES-013)
    #[test]
    fn locked_source_toml_missing_subpath_field_is_none() {
        let toml_str = r#"
version = 1

[dependencies.dep]
url = "https://github.com/org/dep.git"
commit = "deadbeef"
"#;
        let lock: crate::lock::LockFile = toml::from_str(toml_str).unwrap();
        assert!(lock.dependencies["dep"].subpath.is_none());
    }

    // RES-014: LockedSource with subpath serializes the subpath field alongside other fields
    #[test]
    fn locked_source_toml_subpath_serializes_alongside_other_fields() {
        let source = crate::lock::LockedSource {
            url: Some(SourceUrl::from("https://github.com/org/base.git")),
            path: None,
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
            version: Some("v1.0.0".to_string()),
            commit: Some(CommitHash::from("abc123")),
            tree_hash: None,
        };
        #[derive(Serialize)]
        struct Wrapper {
            source: crate::lock::LockedSource,
        }
        let serialized = toml::to_string(&Wrapper { source }).unwrap();
        assert!(serialized.contains("subpath = \"plugins/foo\""));
        assert!(serialized.contains("url = "));
        assert!(serialized.contains("commit = "));
    }

    #[test]
    fn lock_roundtrip_with_and_without_subpath() {
        let old_lock = r#"
version = 1

[dependencies.base]
url = "https://github.com/org/base.git"
"#;
        let parsed_old: crate::lock::LockFile = toml::from_str(old_lock).unwrap();
        assert!(parsed_old.dependencies["base"].subpath.is_none());

        let lock = crate::lock::LockFile {
            version: 1,
            dependencies: indexmap::IndexMap::from([(
                SourceName::from("base"),
                crate::lock::LockedSource {
                    url: Some(SourceUrl::from("https://github.com/org/base.git")),
                    path: None,
                    subpath: Some(SourceSubpath::new(r"plugins\foo").unwrap()),
                    version: Some("v1.2.3".to_string()),
                    commit: Some(CommitHash::from("abc123")),
                    tree_hash: None,
                },
            )]),
            items: indexmap::IndexMap::new(),
        };
        let serialized = toml::to_string_pretty(&lock).unwrap();
        assert!(serialized.contains("subpath = \"plugins/foo\""));
        let reparsed: crate::lock::LockFile = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.dependencies["base"]
                .subpath
                .as_ref()
                .map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    #[test]
    fn config_roundtrip_preserves_subpath() {
        let config = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
subpath = "plugins\\foo"
"#;
        let parsed: crate::config::Config = toml::from_str(config).unwrap();
        assert_eq!(
            parsed.dependencies["base"]
                .subpath
                .as_ref()
                .map(SourceSubpath::as_str),
            Some("plugins/foo")
        );

        let serialized = toml::to_string(&parsed).unwrap();
        assert!(serialized.contains("subpath = \"plugins/foo\""));
        let reparsed: crate::config::Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.dependencies["base"]
                .subpath
                .as_ref()
                .map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    #[test]
    fn source_id_git_same_url_same_subpath_are_equal_and_hash_equal() {
        let a = SourceId::git_with_subpath(
            SourceUrl::from("https://example.com/repo.git"),
            Some(SourceSubpath::new("plugins/foo").unwrap()),
        );
        let b = SourceId::git_with_subpath(
            SourceUrl::from("https://example.com/repo.git"),
            Some(SourceSubpath::new("plugins/foo").unwrap()),
        );

        assert_eq!(a, b);

        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut hasher_b);
        assert_eq!(hasher_a.finish(), hasher_b.finish());
    }

    #[test]
    fn source_id_git_same_url_different_subpaths_are_distinct() {
        let a = SourceId::git_with_subpath(
            SourceUrl::from("https://example.com/repo.git"),
            Some(SourceSubpath::new("plugins/foo").unwrap()),
        );
        let b = SourceId::git_with_subpath(
            SourceUrl::from("https://example.com/repo.git"),
            Some(SourceSubpath::new("plugins/bar").unwrap()),
        );

        assert_ne!(a, b);

        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut hasher_b);
        assert_ne!(hasher_a.finish(), hasher_b.finish());
    }

    // ========== RES-002: SourceId::Path hash stability with subpath ==========

    /// RES-002: SourceId::Path with subpath=None and subpath=Some("plugins/foo")
    /// must hash to distinct values — same canonical path but different subpaths
    /// must not collide.
    #[test]
    fn source_id_path_none_and_some_subpath_hash_distinctly() {
        let canonical = PathBuf::from("/tmp/my-repo");
        let a = SourceId::Path {
            canonical: canonical.clone(),
            subpath: None,
        };
        let b = SourceId::Path {
            canonical: canonical.clone(),
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
        };

        assert_ne!(a, b);

        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut hasher_b);
        assert_ne!(hasher_a.finish(), hasher_b.finish());
    }

    /// RES-002: Two SourceId::Path with the same canonical and same subpath must
    /// be equal and hash equally.
    #[test]
    fn source_id_path_same_canonical_same_subpath_are_equal() {
        let canonical = PathBuf::from("/tmp/my-repo");
        let a = SourceId::Path {
            canonical: canonical.clone(),
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
        };
        let b = SourceId::Path {
            canonical: canonical.clone(),
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
        };

        assert_eq!(a, b);

        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut hasher_b);
        assert_eq!(hasher_a.finish(), hasher_b.finish());
    }

    /// RES-002: Two SourceId::Path with same canonical but different subpaths must
    /// not be equal and must hash differently.
    #[test]
    fn source_id_path_same_canonical_different_subpaths_are_distinct() {
        let canonical = PathBuf::from("/tmp/my-repo");
        let a = SourceId::Path {
            canonical: canonical.clone(),
            subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
        };
        let b = SourceId::Path {
            canonical: canonical.clone(),
            subpath: Some(SourceSubpath::new("plugins/bar").unwrap()),
        };

        assert_ne!(a, b);

        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut hasher_a);
        let mut hasher_b = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut hasher_b);
        assert_ne!(hasher_a.finish(), hasher_b.finish());
    }

    // ========== RES-001: lock file write + load round-trip via lock::write/load ==========

    /// RES-001: A lock file written with lock::write and re-loaded with lock::load
    /// must preserve the subpath field exactly. This exercises the full atomic
    /// write path, not just toml::to_string.
    #[test]
    fn lock_write_and_load_roundtrip_preserves_subpath() {
        use crate::lock::{LockFile, LockedSource};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let lock = LockFile {
            version: 1,
            dependencies: indexmap::IndexMap::from([(
                SourceName::from("dep"),
                LockedSource {
                    url: Some(SourceUrl::from("https://github.com/org/repo.git")),
                    path: None,
                    subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
                    version: Some("v1.2.3".to_string()),
                    commit: Some(CommitHash::from("deadbeef")),
                    tree_hash: None,
                },
            )]),
            items: indexmap::IndexMap::new(),
        };

        crate::lock::write(dir.path(), &lock).unwrap();
        let loaded = crate::lock::load(dir.path()).unwrap();

        assert_eq!(
            loaded.dependencies["dep"]
                .subpath
                .as_ref()
                .map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
        assert_eq!(
            loaded.dependencies["dep"].url.as_deref(),
            Some("https://github.com/org/repo.git")
        );
        assert_eq!(
            loaded.dependencies["dep"].version.as_deref(),
            Some("v1.2.3")
        );
    }

    // ========== RES-001: EffectiveDependency carries subpath after merge ==========

    /// RES-001 (config side): after merge_with_root the EffectiveDependency.subpath
    /// matches what was in the Config.  This confirms the subpath survives the
    /// config-load → merge step.
    #[test]
    fn effective_dependency_subpath_preserved_through_merge() {
        use crate::config::{Config, merge};

        let toml_str = r#"
[dependencies.dep]
url = "https://github.com/org/repo.git"
subpath = "plugins/foo"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let effective = merge(config, crate::config::LocalConfig::default()).unwrap();
        assert_eq!(
            effective.dependencies["dep"]
                .subpath
                .as_ref()
                .map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
        // SourceId must embed the same subpath
        assert!(matches!(
            &effective.dependencies["dep"].id,
            SourceId::Git { subpath: Some(sp), .. } if sp.as_str() == "plugins/foo"
        ));
    }
}
