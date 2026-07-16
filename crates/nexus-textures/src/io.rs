use std::fs::File;
use std::io::{Read, Take};
use std::path::Path;

use crate::BackendFailure;

pub(crate) fn read_file_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, BackendFailure> {
    let file = File::open(path).map_err(|_| BackendFailure::Unavailable)?;
    let limit = u64::try_from(max_bytes)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(BackendFailure::Rejected)?;
    let mut reader: Take<File> = file.take(limit);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| BackendFailure::Unavailable)?;
    if bytes.len() > max_bytes {
        return Err(BackendFailure::Rejected);
    }
    Ok(bytes)
}
