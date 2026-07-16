use core::fmt;
use std::path::{Path, PathBuf};

use crate::io::read_file_bounded;
use crate::{BackendFailure, OverrideProvider};

/// Override provider which reads `<identifier>.png` from one directory.
pub struct DirectoryOverrides {
    directory: PathBuf,
}

impl DirectoryOverrides {
    /// Create a directory-backed provider. The path is never included in `Debug` or errors.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path_for(&self, identifier: &str) -> Option<PathBuf> {
        if identifier == "."
            || identifier == ".."
            || identifier.contains(['/', '\\', ':', '\0'])
            || Path::new(identifier)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(identifier)
        {
            return None;
        }
        Some(self.directory.join(format!("{identifier}.png")))
    }
}

impl fmt::Debug for DirectoryOverrides {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryOverrides")
            .field("directory", &"<redacted>")
            .finish()
    }
}

impl OverrideProvider for DirectoryOverrides {
    fn load_override(
        &self,
        identifier: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, BackendFailure> {
        let Some(path) = self.path_for(identifier) else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        read_file_bounded(&path, max_bytes).map(Some)
    }
}

/// Override provider which never shadows requested sources.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOverrides;

impl OverrideProvider for NoOverrides {
    fn load_override(
        &self,
        _identifier: &str,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, BackendFailure> {
        Ok(None)
    }
}
