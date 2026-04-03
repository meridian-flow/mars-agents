use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::{Path, PathBuf};

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

/// Relative path under the install root (`.agents/` / project root).
#[derive(Eq, PartialEq, Clone, Debug, Ord, PartialOrd)]
pub struct DestPath(PathBuf);

impl DestPath {
    pub fn new(value: impl Into<PathBuf>) -> Self {
        Self(value.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// Resolve this relative path under a root path.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }
}

impl From<PathBuf> for DestPath {
    fn from(value: PathBuf) -> Self {
        Self(value)
    }
}

impl From<&Path> for DestPath {
    fn from(value: &Path) -> Self {
        Self(value.to_path_buf())
    }
}

impl From<&str> for DestPath {
    fn from(value: &str) -> Self {
        Self(PathBuf::from(value))
    }
}

impl From<String> for DestPath {
    fn from(value: String) -> Self {
        Self(PathBuf::from(value))
    }
}

impl AsRef<Path> for DestPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Borrow<Path> for DestPath {
    fn borrow(&self) -> &Path {
        &self.0
    }
}

impl Borrow<str> for DestPath {
    fn borrow(&self) -> &str {
        self.0.to_str().expect("DestPath must be valid UTF-8")
    }
}

impl Hash for DestPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_string_lossy().hash(state);
    }
}

impl Deref for DestPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for DestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Serialize for DestPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.to_string_lossy().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DestPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|s| Self(PathBuf::from(s)))
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
#[derive(Hash, Eq, PartialEq, Clone, Debug, Ord, PartialOrd)]
pub enum SourceId {
    Git { url: SourceUrl },
    Path { canonical: PathBuf },
}

impl SourceId {
    pub fn git(url: SourceUrl) -> Self {
        Self::Git { url }
    }

    pub fn path(base: &Path, relative_or_absolute: &Path) -> std::io::Result<Self> {
        let candidate = if relative_or_absolute.is_absolute() {
            relative_or_absolute.to_path_buf()
        } else {
            base.join(relative_or_absolute)
        };
        let canonical = candidate.canonicalize()?;
        Ok(Self::Path { canonical })
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git { url } => write!(f, "git:{url}"),
            Self::Path { canonical } => write!(f, "path:{}", canonical.display()),
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
}
