use core::cell::UnsafeCell;
use core::num::NonZeroUsize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::mapping::{MappingBackend, MappingDisposition, MappingFailure, MappingView};

#[derive(Default)]
pub(crate) struct MemoryMappingBackend {
    mappings: Mutex<HashMap<String, Arc<MemoryRegion>>>,
}

struct MemoryRegion {
    bytes: Box<[UnsafeCell<u8>]>,
}

// SAFETY: this test-only type models externally synchronized shared memory.
// Tests retain the allocation and coordinate all raw access explicitly.
unsafe impl Send for MemoryRegion {}
// SAFETY: see the `Send` justification; mutation occurs only through the
// shared-memory raw-pointer contract exercised by each test.
unsafe impl Sync for MemoryRegion {}

struct MemoryView {
    region: Arc<MemoryRegion>,
    disposition: MappingDisposition,
}

// SAFETY: `region` owns a non-empty fixed boxed slice, so the reported address
// is stable, writable, exact-length, and retained for the full view lifetime.
unsafe impl MappingView for MemoryView {
    fn address(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.region.bytes.as_ptr() as usize)
            .expect("a non-empty boxed slice has a non-null address")
    }

    fn len(&self) -> usize {
        self.region.bytes.len()
    }

    fn disposition(&self) -> MappingDisposition {
        self.disposition
    }
}

impl MappingBackend for MemoryMappingBackend {
    fn open_or_create(
        &self,
        name: &str,
        size: NonZeroUsize,
    ) -> Result<Arc<dyn MappingView>, MappingFailure> {
        let mut mappings = self
            .mappings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (region, disposition) = match mappings.get(name) {
            Some(region) => (Arc::clone(region), MappingDisposition::OpenedExisting),
            None => {
                let bytes = (0..size.get())
                    .map(|_| UnsafeCell::new(0_u8))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                let region = Arc::new(MemoryRegion { bytes });
                mappings.insert(name.to_owned(), Arc::clone(&region));
                (region, MappingDisposition::CreatedNew)
            }
        };
        Ok(Arc::new(MemoryView {
            region,
            disposition,
        }))
    }
}

pub(crate) struct WrongLengthBackend;

impl MappingBackend for WrongLengthBackend {
    fn open_or_create(
        &self,
        _name: &str,
        size: NonZeroUsize,
    ) -> Result<Arc<dyn MappingView>, MappingFailure> {
        let wrong_size = NonZeroUsize::new(size.get().saturating_sub(1))
            .ok_or(MappingFailure::SizeUnsupported)?;
        let backend = MemoryMappingBackend::default();
        backend.open_or_create("wrong-length", wrong_size)
    }
}
