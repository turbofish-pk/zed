//! Relative path types for deltadb.
//!
//! Provides [`RelPath`] and [`RelPathBuf`] — path types that are guaranteed to be
//! relative, normalized, and valid unicode. Internally stored in POSIX (`/`-delimited)
//! format regardless of host platform.
//!
//! Adapted from Zed's `util::rel_path` module.

#[cfg(windows)]
use anyhow::Context;
#[cfg(all(unix, not(target_family = "wasm")))]
use std::os::unix::prelude::OsStrExt;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};
#[cfg(windows)]
use tendril::fmt::{Format, WTF8};

use crate::path_component::{Component, Components, Prefix, PrefixExt, iter_after};
use crate::rel_path::{RelPath, StripPrefixError};

pub mod abs_path;
mod path_component;
pub mod rel_path;
mod windows_prefix;

pub fn try_from_bytes<'a, T>(bytes: &'a [u8]) -> anyhow::Result<T>
where
    T: From<&'a Path>,
{
    #[cfg(target_family = "wasm")]
    {
        std::str::from_utf8(bytes)
            .map(Path::new)
            .map(Into::into)
            .map_err(Into::into)
    }
    #[cfg(all(unix, not(target_family = "wasm")))]
    {
        Ok(T::from(Path::new(OsStr::from_bytes(bytes))))
    }
    #[cfg(windows)]
    {
        WTF8::validate(bytes)
            .then(|| {
                // Safety: bytes are valid WTF-8 sequence.
                T::from(Path::new(unsafe {
                    OsStr::from_encoded_bytes_unchecked(bytes)
                }))
            })
            .with_context(|| format!("Invalid WTF-8 sequence: {bytes:?}"))
    }
}

pub trait PathExt {
    fn to_rel_path_buf(&self) -> anyhow::Result<rel_path::RelPathBuf>;
}

impl<T: AsRef<Path> + ?Sized> PathExt for T {
    fn to_rel_path_buf(&self) -> anyhow::Result<rel_path::RelPathBuf> {
        Ok(RelPath::new(self.as_ref(), PathStyle::local())?.into_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathStyle {
    Unix,
    Windows,
}

impl PathStyle {
    #[cfg(target_os = "windows")]
    pub const fn local() -> Self {
        PathStyle::Windows
    }

    
    pub const fn local() -> Self {
        PathStyle::Unix
    }

    #[inline]
    pub fn primary_separator(&self) -> &'static str {
        match self {
            PathStyle::Unix => "/",
            PathStyle::Windows => "\\",
        }
    }

    pub fn separators(&self) -> &'static [&'static str] {
        match self {
            PathStyle::Unix => &["/"],
            PathStyle::Windows => &["\\", "/"],
        }
    }

    pub fn separators_ch(&self) -> &'static [char] {
        match self {
            PathStyle::Unix => &['/'],
            PathStyle::Windows => &['\\', '/'],
        }
    }

    pub fn is_absolute(&self, path: &Path) -> bool {
        match self {
            PathStyle::Unix => self.has_root(path),
            PathStyle::Windows => self.has_root(path) && self.components(path).prefix().is_some(),
        }
    }

    pub fn has_root(&self, path: &Path) -> bool {
        self.components(path).has_root()
    }

    pub fn is_windows(&self) -> bool {
        *self == PathStyle::Windows
    }

    pub fn is_posix(&self) -> bool {
        *self == PathStyle::Unix
    }

    pub fn join(self, left: impl AsRef<Path>, right: impl AsRef<Path>) -> Option<String> {
        let right = right.as_ref();
        if self.has_root(right) {
            return None;
        }
        let right = right.to_str()?;
        let left = left.as_ref().to_str()?;
        if left.is_empty() {
            Some(right.into())
        } else {
            Some(format!(
                "{left}{}{right}",
                if left.ends_with(self.primary_separator()) {
                    ""
                } else {
                    self.primary_separator()
                }
            ))
        }
    }

    pub fn join_path(
        self,
        left: impl AsRef<Path>,
        right: impl AsRef<Path>,
    ) -> anyhow::Result<PathBuf> {
        let left = left
            .as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;
        let right = self.normalize_separators(right);
        let right_string = right
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;
        let joined = self
            .join(left, right_string)
            .ok_or_else(|| anyhow::anyhow!("Path must be relative: {right:?}"))?;
        Ok(self.normalize_lexically(&joined))
    }

    pub fn join_path_preserving_components(
        self,
        left: impl AsRef<Path>,
        right: impl AsRef<Path>,
    ) -> anyhow::Result<PathBuf> {
        let left = left
            .as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;
        let right = right.as_ref();
        let right_string = right
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;
        let joined = self
            .join(left, right_string)
            .ok_or_else(|| anyhow::anyhow!("Path must be relative: {right:?}"))?;
        Ok(PathBuf::from(&joined))
    }

    pub fn normalize_separators(&self, path: impl AsRef<Path>) -> PathBuf {
        self.collect_components(self.components(path.as_ref()))
    }

    pub fn normalize_lexically(self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        let mut normalized: Vec<Component<'_>> = Vec::new();

        for component in self.components(path) {
            match component {
                Component::CurDir => {}
                Component::ParentDir => match normalized.last() {
                    Some(Component::Normal(_)) => {
                        normalized.pop();
                    }
                    // There is no parent above a root, so discard the attempt.
                    Some(Component::RootDir) => {}
                    _ => normalized.push(component),
                },
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    normalized.push(component);
                }
            }
        }

        if normalized.is_empty() && !path.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            self.collect_components(normalized)
        }
    }

    pub fn file_name<'a>(&self, path: &'a Path) -> Option<&'a OsStr> {
        self.components(path).next_back().and_then(|p| match p {
            Component::Normal(p) => Some(p),
            _ => None,
        })
    }

    pub fn parent<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let mut comps = self.components(path);
        let comp = comps.next_back();
        comp.and_then(|p| match p {
            Component::Normal(_) | Component::CurDir | Component::ParentDir => {
                Some(comps.as_path())
            }
            _ => None,
        })
    }

    pub fn starts_with(&self, path: impl AsRef<Path>, prefix: impl AsRef<Path>) -> bool {
        iter_after(
            self.components(path.as_ref()),
            self.components(prefix.as_ref()),
        )
        .is_some()
    }

    pub fn strip_prefix<'a>(
        &self,
        path: &'a (impl AsRef<Path> + ?Sized),
        prefix: impl AsRef<Path>,
    ) -> Result<&'a Path, StripPrefixError> {
        iter_after(
            self.components(path.as_ref()),
            self.components(prefix.as_ref()),
        )
        .map(|c| c.as_path())
        .ok_or(StripPrefixError)
    }
}

// This part contains the methods adopted from the std lib
// to make the pub api of `PathStyle` compatible with `Path`
impl PathStyle {
    fn components<'a>(&self, path: &'a Path) -> Components<'a> {
        Components::new(*self, path)
    }

    fn parse_prefix<'a>(&self, path: &'a OsStr) -> Option<Prefix<'a>> {
        match self {
            PathStyle::Unix => None,
            PathStyle::Windows => windows_prefix::parse_prefix(path),
        }
    }

    fn has_prefixes(&self) -> bool {
        match self {
            PathStyle::Unix => false,
            PathStyle::Windows => true,
        }
    }

    fn is_sep_byte(&self, b: u8) -> bool {
        match self {
            Self::Unix => b == b'/',
            Self::Windows => b == b'\\' || b == b'/',
        }
    }

    fn is_verbatim_sep(&self, b: u8) -> bool {
        match self {
            Self::Unix => self.is_sep_byte(b),
            Self::Windows => b == b'\\',
        }
    }

    fn has_physical_root(&self, s: &[u8], prefix: Option<Prefix<'_>>) -> bool {
        let path = if let Some(p) = prefix {
            &s[p.len()..]
        } else {
            s
        };
        !path.is_empty() && self.is_sep_byte(path[0])
    }

    /// Collects `components` into a `PathBuf`, replacing std's
    /// `impl<P: AsRef<Path>> FromIterator<P> for PathBuf` (which is implemented
    /// in terms of `PathBuf::push`) with a [`PathStyle`]-aware version.
    ///
    /// Unlike std's implementation, this appends each component's raw `OsStr`
    /// rather than pushing it as a standalone path. That distinction matters for
    /// Windows verbatim paths: `\\?\C:\a/b` has a single `Normal("a/b")`
    /// component, std lib is buggy here (rust-lang/rust#164467)
    fn collect_components<'a>(
        self,
        components: impl IntoIterator<Item = Component<'a>>,
    ) -> PathBuf {
        let mut result = OsString::new();
        let mut need_sep = false;

        for component in components {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    result.push(component.as_os_str(self));
                    need_sep = false;
                }
                _ => {
                    if need_sep {
                        result.push(self.primary_separator());
                    }
                    result.push(component.as_os_str(self));
                    need_sep = true;
                }
            }
        }

        PathBuf::from(result)
    }
}

