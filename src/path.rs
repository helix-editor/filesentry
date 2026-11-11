use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::mem::transmute;
use std::ops::Deref;
use std::path::Path;

#[cfg(unix)]
const PATH_SEPARATOR: u8 = b'/';
#[cfg(windows)]
const PATH_SEPARATOR: u8 = b'\\';

use ecow::EcoVec;
use memchr::memrchr;

#[repr(transparent)]
#[derive(PartialEq, Eq)]
pub struct CannonicalPath {
    bytes: [u8],
}

#[cfg(unix)]
const EMPTY: &[u8] = b".\0";
#[cfg(windows)]
const EMPTY: &[u8] = b".";

impl CannonicalPath {
    pub fn as_std_path(&self) -> &Path {
        // safety: type ensures that self.buf is composition of
        // OsStr (and str but every str is an OsStr) and therefore always
        // valid
        Path::new(self.as_os_str())
    }

    pub fn parent(&self) -> Option<&Path> {
        let i = memrchr(PATH_SEPARATOR, &self.bytes)?;
        // safety: type ensures that self.buf is composition of
        // OsStr (and str but every str is an OsStr) and therefore always
        // valid
        let path = unsafe { OsStr::from_encoded_bytes_unchecked(&self.bytes[..i]) };
        Some(Path::new(path))
    }

    pub fn join(&self, other: &OsStr) -> CanonicalPathBuf {
        let mut res = CanonicalPathBuf {
            buf: EcoVec::with_capacity(self.bytes.len() + other.len() + 2),
        };
        res.buf.extend_from_slice(&self.bytes);
        res.push(other);
        res
    }

    fn as_raw_bytes(&self) -> &[u8] {
        if self.bytes.is_empty() {
            EMPTY
        } else {
            &self.bytes
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        let bytes = self.as_raw_bytes();
        if cfg!(unix) {
            &bytes[..bytes.len() - 1]
        } else {
            bytes
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn len(&self) -> usize {
        if cfg!(unix) {
            self.bytes.len().saturating_sub(1)
        } else {
            self.bytes.len()
        }
    }

    pub fn as_os_str(&self) -> &OsStr {
        // safety: type ensures that self.buf is composition of
        // OsStr (and str but every str is an OsStr) and therefore always
        // valid
        unsafe { OsStr::from_encoded_bytes_unchecked(self.as_bytes()) }
    }

    /// NUL-terminated UTF-16, for Win32 calls.
    #[cfg(windows)]
    pub(crate) fn to_wide(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    #[cfg(unix)]
    pub fn as_c_str(&self) -> &std::ffi::CStr {
        // safety: type is always null terminated by construction
        unsafe { std::ffi::CStr::from_bytes_with_nul_unchecked(self.as_raw_bytes()) }
    }

    pub fn is_parent_of(&self, other: &CannonicalPath) -> bool {
        other.as_bytes().starts_with(self.as_bytes())
            && other.bytes.get(self.len()) == Some(&PATH_SEPARATOR)
    }
}

/// A custom PathBuf type that has some desirable properties:
///
/// * only 2 words size reducing memory pressure
/// * reference counted
/// * mutation via copy on write
/// * always canonicalized enabling fast byte-wise comparisons
/// * never ends with a path separator
#[derive(PartialEq, Eq, Clone)]
pub struct CanonicalPathBuf {
    buf: EcoVec<u8>,
}

impl PartialOrd for CanonicalPathBuf {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CanonicalPathBuf {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp(&self.buf, &other.buf)
    }
}

/// Total order over canonical paths that also yields "tree order": a directory
/// sorts immediately before its descendants, and a path's descendants sort
/// before siblings that share its prefix. Ranking the path separator below every
/// other byte (and end-of-path below the separator, so a parent sorts first)
/// makes this a plain lexicographic compare over an injective rank — a provable
/// total order, unlike the hand-rolled comparator it replaced.
fn cmp(lhs: &[u8], rhs: &[u8]) -> Ordering {
    let lhs = strip_nul(lhs);
    let rhs = strip_nul(rhs);
    let common = lhs.len().min(rhs.len());
    for i in 0..common {
        if lhs[i] != rhs[i] {
            return rank(lhs[i]).cmp(&rank(rhs[i]));
        }
    }
    // Shared prefix is equal; the shorter path (the prefix/parent) sorts first.
    lhs.len().cmp(&rhs.len())
}

/// Rank a byte so the path separator sorts before every other byte.
#[inline]
fn rank(byte: u8) -> u16 {
    if byte == PATH_SEPARATOR {
        0
    } else {
        byte as u16 + 1
    }
}

/// Strip the trailing NUL terminator that `CanonicalPathBuf` keeps on unix.
#[inline]
fn strip_nul(buf: &[u8]) -> &[u8] {
    if cfg!(unix) && buf.last() == Some(&0) {
        &buf[..buf.len() - 1]
    } else {
        buf
    }
}

impl CanonicalPathBuf {
    pub fn new() -> CanonicalPathBuf {
        Self { buf: EcoVec::new() }
    }

    pub fn assert_canonicalized(path: &Path) -> CanonicalPathBuf {
        let path = path.as_os_str();
        let mut res = Self::new();
        res.push(path);
        res
    }

    pub fn push(&mut self, src: impl AsRef<OsStr>) {
        let src = src.as_ref().as_encoded_bytes();
        // only unix paths carry a NUL terminator
        if cfg!(unix) {
            let removed = self.buf.pop();
            debug_assert!(removed.is_none_or(|c| c == 0));
        }
        self.buf.reserve(src.len() + 2);
        if src.first() != Some(&PATH_SEPARATOR) {
            self.buf.push(PATH_SEPARATOR);
        }
        self.buf.extend_from_slice(src);
        if cfg!(unix) {
            self.buf.push(0);
        }
    }
}

impl Default for CanonicalPathBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for CanonicalPathBuf {
    type Target = CannonicalPath;

    fn deref(&self) -> &Self::Target {
        // safety: repr(transparent)
        unsafe { transmute(self.buf.as_slice()) }
    }
}

impl Debug for CanonicalPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_std_path().fmt(f)
    }
}

impl Display for CanonicalPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.as_std_path().display(), f)
    }
}

impl Debug for CannonicalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_std_path().fmt(f)
    }
}

impl Display for CannonicalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.as_std_path().display(), f)
    }
}

#[cfg(unix)]
impl rustix::path::Arg for &CannonicalPath {
    fn as_str(&self) -> rustix::io::Result<&str> {
        self.as_os_str().to_str().ok_or(rustix::io::Errno::INVAL)
    }

    fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.as_std_path().to_string_lossy()
    }

    fn as_cow_c_str(&self) -> rustix::io::Result<std::borrow::Cow<'_, std::ffi::CStr>> {
        Ok(self.as_c_str().into())
    }

    fn into_c_str<'b>(self) -> rustix::io::Result<std::borrow::Cow<'b, std::ffi::CStr>>
    where
        Self: 'b,
    {
        Ok(unsafe {
            use std::ffi::CString;
            CString::from_vec_with_nul_unchecked(Vec::from(&self.bytes)).into()
        })
    }

    fn into_with_c_str<T, F>(self, f: F) -> rustix::io::Result<T>
    where
        Self: Sized,
        F: FnOnce(&std::ffi::CStr) -> rustix::io::Result<T>,
    {
        f(self.as_c_str())
    }
}

// don't include the null terminator for Hash so that we
// can lookup a normal path as well
impl Hash for CannonicalPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl Hash for CanonicalPathBuf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl<T: AsRef<OsStr>> PartialEq<T> for CannonicalPath {
    fn eq(&self, other: &T) -> bool {
        self.as_os_str() == other.as_ref()
    }
}

impl<T: AsRef<OsStr>> PartialEq<T> for CanonicalPathBuf {
    fn eq(&self, other: &T) -> bool {
        self.as_os_str() == other.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a path from a `/`-separated string, rewriting separators to the
    /// platform's so these byte-level order/parent assertions hold on Windows
    /// (where `PATH_SEPARATOR` is `\\`, not `/`) as well as on unix.
    fn p(s: &str) -> CanonicalPathBuf {
        let s = s.replace('/', std::path::MAIN_SEPARATOR_STR);
        CanonicalPathBuf::assert_canonicalized(Path::new(&s))
    }

    #[test]
    fn is_parent_of() {
        assert!(p("/foo").is_parent_of(&p("/foo/bar")));
        assert!(p("/foo").is_parent_of(&p("/foo/bar/baz")));
        // not a prefix that ends inside a name, not the path itself or a child
        assert!(!p("/foo").is_parent_of(&p("/foobar")));
        assert!(!p("/foo").is_parent_of(&p("/foo")));
        assert!(!p("/foo/bar").is_parent_of(&p("/foo")));
    }

    #[test]
    fn tree_order() {
        assert!(p("/foo") < p("/foo/bar")); // parent before child
        assert!(p("/foo") < p("/foobar")); // prefix before longer sibling
        assert!(p("/foo/bar") < p("/foo/baz")); // siblings by name
        assert!(p("/foo/a") < p("/foo-x")); // child of foo before sibling foo-x
        assert!(p("/foo/bar") < p("/foobar")); // descendant before prefix-sibling
    }

    #[test]
    fn total_order_axioms() {
        // a directory, its descendants and prefix-sharing siblings
        let set: Vec<_> = [
            "/foo", "/foo/a", "/foo/b", "/foo/a/x", "/foo-x", "/foobar", "/bar", "/bar/baz", "/a",
            "/a/b/c", "/foo/a/y", "/foo/aa",
        ]
        .into_iter()
        .map(p)
        .collect();

        for a in &set {
            // reflexive + antisymmetric
            assert_eq!(a.cmp(a), Ordering::Equal);
            for b in &set {
                assert_eq!(a.cmp(b), b.cmp(a).reverse(), "antisymmetry {a:?} {b:?}");
                for c in &set {
                    // transitivity of <=
                    if a.cmp(b) != Ordering::Greater && b.cmp(c) != Ordering::Greater {
                        assert_ne!(
                            a.cmp(c),
                            Ordering::Greater,
                            "transitivity {a:?} {b:?} {c:?}"
                        );
                    }
                }
            }
        }
    }
}
