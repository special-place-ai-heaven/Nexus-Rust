use std::{
    fmt,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
};

use nexus_addon_loader::{AbsoluteDllPath, PathPolicyError};
use thiserror::Error;

/// Size of the legacy MD5 revision stored by `AddonConfig.json`.
const REVISION_BYTES: usize = 16;

/// An absolute, lexically normalized directory containing add-on DLLs.
// Deliberately omits `Display`; diagnostics must never reveal host paths.
#[derive(Clone, Eq, PartialEq)]
pub struct AddonDirectory(PathBuf);

impl AddonDirectory {
    /// Validates an absolute directory path without touching the filesystem.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, DiscoveryError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(DiscoveryError::DirectoryNotAbsolute);
        }
        if path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(DiscoveryError::DirectoryNotNormalized);
        }
        Ok(Self(path.to_path_buf()))
    }

    /// Borrows the validated directory path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn contains_direct_child(&self, path: &AbsoluteDllPath) -> bool {
        path.as_path().parent() == Some(self.as_path())
    }
}

impl fmt::Debug for AddonDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AddonDirectory(<redacted>)")
    }
}

/// Stable legacy-compatible content revision for one DLL.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BinaryRevision([u8; REVISION_BYTES]);

impl BinaryRevision {
    /// Creates a revision from the exact legacy digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; REVISION_BYTES]) -> Self {
        Self(bytes)
    }

    /// Returns the digest bytes.
    #[must_use]
    pub const fn bytes(self) -> [u8; REVISION_BYTES] {
        self.0
    }

    /// Formats the revision using the lowercase legacy MD5 representation.
    #[must_use]
    pub fn legacy_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(REVISION_BYTES * 2);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }

    /// Compares a legacy hexadecimal revision without exposing it in diagnostics.
    #[must_use]
    pub fn matches_legacy_hex(self, value: &str) -> bool {
        value.len() == REVISION_BYTES * 2 && self.legacy_hex().eq_ignore_ascii_case(value)
    }
}

impl fmt::Debug for BinaryRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BinaryRevision(<redacted>)")
    }
}

/// One inert DLL discovered on disk.
#[derive(Clone, Eq, PartialEq)]
pub struct DiscoveredDll {
    path: AbsoluteDllPath,
    revision: BinaryRevision,
    byte_len: u64,
}

impl DiscoveredDll {
    /// Creates a discovery record without loading or executing the DLL.
    pub fn new(
        path: impl AsRef<Path>,
        revision: BinaryRevision,
        byte_len: u64,
    ) -> Result<Self, DiscoveryError> {
        if byte_len == 0 {
            return Err(DiscoveryError::EmptyBinary);
        }
        let path = AbsoluteDllPath::new(path).map_err(DiscoveryError::PathPolicy)?;
        Ok(Self {
            path,
            revision,
            byte_len,
        })
    }

    /// Borrows the validated absolute DLL path.
    #[must_use]
    pub const fn path(&self) -> &AbsoluteDllPath {
        &self.path
    }

    /// Returns the content revision captured during discovery.
    #[must_use]
    pub const fn revision(&self) -> BinaryRevision {
        self.revision
    }

    /// Returns the observed file length.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

impl fmt::Debug for DiscoveredDll {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredDll")
            .field("path", &self.path)
            .field("revision", &self.revision)
            .field("byte_len", &self.byte_len)
            .finish()
    }
}

/// Injected, inert directory scan boundary.
pub trait DirectoryScanner: Send + Sync + 'static {
    /// Returns candidate DLLs without loading or executing any of them.
    fn scan(&self, directory: &AddonDirectory) -> Result<Vec<DiscoveredDll>, DiscoveryError>;
}

/// Standard immediate-directory scanner with legacy MD5 revisions.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdDirectoryScanner;

impl DirectoryScanner for StdDirectoryScanner {
    fn scan(&self, directory: &AddonDirectory) -> Result<Vec<DiscoveredDll>, DiscoveryError> {
        let entries =
            fs::read_dir(directory.as_path()).map_err(|_error| DiscoveryError::ReadDirectory)?;
        let mut discovered = Vec::new();

        for entry in entries {
            let entry = entry.map_err(|_error| DiscoveryError::InspectEntry)?;
            let file_type = entry
                .file_type()
                .map_err(|_error| DiscoveryError::InspectEntry)?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            let is_dll = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));
            if !is_dll {
                continue;
            }

            let metadata = entry
                .metadata()
                .map_err(|_error| DiscoveryError::InspectEntry)?;
            if metadata.len() == 0 {
                continue;
            }
            let revision = revision_from_file(&path)?;
            discovered.push(DiscoveredDll::new(path, revision, metadata.len())?);
        }

        normalize_discovery(directory, discovered)
    }
}

/// An injected watcher notification. Applying one never loads native code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryEvent {
    /// A candidate was created or its content changed.
    Upsert(DiscoveredDll),
    /// A candidate disappeared from the watched directory.
    Removed(AbsoluteDllPath),
    /// A candidate moved within the watched directory.
    Renamed {
        /// Previous absolute DLL path.
        from: AbsoluteDllPath,
        /// New inert discovery record.
        to: DiscoveredDll,
    },
    /// The watcher lost detail and requests a complete injected scan.
    Rescan,
}

/// Closed discovery failures that never contain a filesystem path.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DiscoveryError {
    /// The configured add-on directory was relative.
    #[error("add-on directory must be absolute")]
    DirectoryNotAbsolute,
    /// The configured directory contained `.` or `..` components.
    #[error("add-on directory must be lexically normalized")]
    DirectoryNotNormalized,
    /// A candidate path was not accepted by the DLL path policy.
    #[error("candidate path violates the add-on DLL policy")]
    PathPolicy(#[source] PathPolicyError),
    /// A scanner returned a path outside the immediate add-on directory.
    #[error("candidate is outside the immediate add-on directory")]
    OutsideDirectory,
    /// A scanner returned the same case-insensitive path more than once.
    #[error("scanner returned a duplicate add-on path")]
    DuplicateEntry,
    /// A zero-length DLL is never a valid candidate.
    #[error("candidate DLL is empty")]
    EmptyBinary,
    /// The directory could not be enumerated.
    #[error("add-on directory could not be read")]
    ReadDirectory,
    /// A directory entry could not be inspected.
    #[error("add-on directory entry could not be inspected")]
    InspectEntry,
    /// A DLL could not be read to calculate its legacy revision.
    #[error("candidate DLL could not be fingerprinted")]
    ReadBinary,
}

pub(crate) fn normalize_discovery(
    directory: &AddonDirectory,
    mut entries: Vec<DiscoveredDll>,
) -> Result<Vec<DiscoveredDll>, DiscoveryError> {
    for entry in &entries {
        if !directory.contains_direct_child(entry.path()) {
            return Err(DiscoveryError::OutsideDirectory);
        }
    }
    entries.sort_by_key(|entry| path_key(entry.path()));
    if entries
        .windows(2)
        .any(|pair| path_key(pair[0].path()) == path_key(pair[1].path()))
    {
        return Err(DiscoveryError::DuplicateEntry);
    }
    Ok(entries)
}

pub(crate) fn path_key(path: &AbsoluteDllPath) -> String {
    path.as_path()
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

fn revision_from_file(path: &Path) -> Result<BinaryRevision, DiscoveryError> {
    let file = File::open(path).map_err(|_error| DiscoveryError::ReadBinary)?;
    let mut reader = BufReader::new(file);
    let mut digest = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_error| DiscoveryError::ReadBinary)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(BinaryRevision::from_bytes(digest.finalize()))
}

struct Md5 {
    state: [u32; 4],
    byte_len: u64,
    buffer: [u8; 64],
    buffered: usize,
}

impl Md5 {
    const fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            byte_len: 0,
            buffer: [0; 64],
            buffered: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.byte_len = self.byte_len.wrapping_add(input.len() as u64);
        if self.buffered != 0 {
            let take = (64 - self.buffered).min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered < 64 {
                return;
            }
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while input.len() >= 64 {
            let (block, rest) = input.split_at(64);
            let block: &[u8; 64] = block
                .try_into()
                .expect("a 64-byte split always converts to an array reference");
            self.compress(block);
            input = rest;
        }
        self.buffer[..input.len()].copy_from_slice(input);
        self.buffered = input.len();
    }

    fn finalize(mut self) -> [u8; REVISION_BYTES] {
        let bit_len = self.byte_len.wrapping_mul(8);
        let mut tail = [0_u8; 128];
        tail[..self.buffered].copy_from_slice(&self.buffer[..self.buffered]);
        tail[self.buffered] = 0x80;
        let tail_len = if self.buffered < 56 { 64 } else { 128 };
        tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_le_bytes());
        for block in tail[..tail_len].chunks_exact(64) {
            let block: &[u8; 64] = block
                .try_into()
                .expect("a 64-byte chunk always converts to an array reference");
            self.compress(block);
        }

        let mut output = [0_u8; REVISION_BYTES];
        for (chunk, value) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        output
    }

    #[allow(clippy::many_single_char_names)]
    fn compress(&mut self, block: &[u8; 64]) {
        const SHIFT: [u32; 64] = [
            7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
            5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
            6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
        ];
        const CONSTANT: [u32; 64] = [
            0xd76a_a478,
            0xe8c7_b756,
            0x2420_70db,
            0xc1bd_ceee,
            0xf57c_0faf,
            0x4787_c62a,
            0xa830_4613,
            0xfd46_9501,
            0x6980_98d8,
            0x8b44_f7af,
            0xffff_5bb1,
            0x895c_d7be,
            0x6b90_1122,
            0xfd98_7193,
            0xa679_438e,
            0x49b4_0821,
            0xf61e_2562,
            0xc040_b340,
            0x265e_5a51,
            0xe9b6_c7aa,
            0xd62f_105d,
            0x0244_1453,
            0xd8a1_e681,
            0xe7d3_fbc8,
            0x21e1_cde6,
            0xc337_07d6,
            0xf4d5_0d87,
            0x455a_14ed,
            0xa9e3_e905,
            0xfcef_a3f8,
            0x676f_02d9,
            0x8d2a_4c8a,
            0xfffa_3942,
            0x8771_f681,
            0x6d9d_6122,
            0xfde5_380c,
            0xa4be_ea44,
            0x4bde_cfa9,
            0xf6bb_4b60,
            0xbebf_bc70,
            0x289b_7ec6,
            0xeaa1_27fa,
            0xd4ef_3085,
            0x0488_1d05,
            0xd9d4_d039,
            0xe6db_99e5,
            0x1fa2_7cf8,
            0xc4ac_5665,
            0xf429_2244,
            0x432a_ff97,
            0xab94_23a7,
            0xfc93_a039,
            0x655b_59c3,
            0x8f0c_cc92,
            0xffef_f47d,
            0x8584_5dd1,
            0x6fa8_7e4f,
            0xfe2c_e6e0,
            0xa301_4314,
            0x4e08_11a1,
            0xf753_7e82,
            0xbd3a_f235,
            0x2ad7_d2bb,
            0xeb86_d391,
        ];

        let mut words = [0_u32; 16];
        for (word, bytes) in words.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_le_bytes(
                bytes
                    .try_into()
                    .expect("a four-byte chunk always converts to an array"),
            );
        }

        let [mut a, mut b, mut c, mut d] = self.state;
        for index in 0..64 {
            let (mixed, word_index) = match index {
                0..=15 => ((b & c) | ((!b) & d), index),
                16..=31 => ((d & b) | ((!d) & c), (5 * index + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let next = a
                .wrapping_add(mixed)
                .wrapping_add(CONSTANT[index])
                .wrapping_add(words[word_index]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(next.rotate_left(SHIFT[index]));
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::{AddonDirectory, BinaryRevision, DiscoveredDll, Md5, normalize_discovery};
    use std::path::PathBuf;

    fn absolute(path: &str) -> PathBuf {
        #[cfg(target_os = "windows")]
        {
            PathBuf::from(format!(r"C:\NexusTests\{path}"))
        }
        #[cfg(not(target_os = "windows"))]
        {
            PathBuf::from(format!("/nexus-tests/{path}"))
        }
    }

    #[test]
    fn md5_matches_published_legacy_vectors() {
        for (input, expected) in [
            (b"".as_slice(), "d41d8cd98f00b204e9800998ecf8427e"),
            (b"abc".as_slice(), "900150983cd24fb0d6963f7d28e17f72"),
            (
                b"The quick brown fox jumps over the lazy dog".as_slice(),
                "9e107d9d372bb6826bd81d3542a419d6",
            ),
        ] {
            let mut md5 = Md5::new();
            for chunk in input.chunks(3) {
                md5.update(chunk);
            }
            assert_eq!(
                BinaryRevision::from_bytes(md5.finalize()).legacy_hex(),
                expected
            );
        }
    }

    #[test]
    fn discovery_is_case_insensitively_sorted_and_rejects_duplicates() {
        let directory = AddonDirectory::new(absolute("addons")).expect("directory is absolute");
        let revision = BinaryRevision::from_bytes([1; 16]);
        let make = |name: &str| {
            DiscoveredDll::new(absolute(&format!("addons/{name}")), revision, 1)
                .expect("fixture is a DLL")
        };

        let sorted = normalize_discovery(
            &directory,
            vec![make("Zulu.dll"), make("alpha.dll"), make("Beta.DLL")],
        )
        .expect("entries should normalize");
        let names: Vec<_> = sorted
            .iter()
            .filter_map(|entry| entry.path().as_path().file_name())
            .collect();
        assert_eq!(names, ["alpha.dll", "Beta.DLL", "Zulu.dll"]);

        assert!(normalize_discovery(&directory, vec![make("same.dll"), make("SAME.dll")]).is_err());
    }
}
