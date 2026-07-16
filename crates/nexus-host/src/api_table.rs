use std::{
    mem::{align_of, size_of},
    pin::Pin,
    ptr::{NonNull, copy_nonoverlapping, from_ref},
};

use nexus_abi::{AddonApi, AddonApiV1, AddonApiV2, AddonApiV3, AddonApiV4, AddonApiV5, AddonApiV6};
use thiserror::Error;

/// A supported Nexus add-on API revision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u32)]
pub enum ApiRevision {
    /// Flat API revision 1.
    V1 = 1,
    /// Flat API revision 2.
    V2 = 2,
    /// Flat API revision 3.
    V3 = 3,
    /// Flat API revision 4.
    V4 = 4,
    /// Flat API revision 5.
    V5 = 5,
    /// Grouped API revision 6.
    V6 = 6,
}

impl ApiRevision {
    /// Every API revision supported by the legacy add-on ABI.
    pub const ALL: [Self; 6] = [Self::V1, Self::V2, Self::V3, Self::V4, Self::V5, Self::V6];

    /// Returns the ABI layout for this revision.
    #[must_use]
    pub const fn layout(self) -> ApiTableLayout {
        match self {
            Self::V1 => ApiTableLayout::of::<AddonApiV1>(),
            Self::V2 => ApiTableLayout::of::<AddonApiV2>(),
            Self::V3 => ApiTableLayout::of::<AddonApiV3>(),
            Self::V4 => ApiTableLayout::of::<AddonApiV4>(),
            Self::V5 => ApiTableLayout::of::<AddonApiV5>(),
            Self::V6 => ApiTableLayout::of::<AddonApiV6>(),
        }
    }

    const fn index(self) -> usize {
        self as usize - 1
    }
}

impl TryFrom<u32> for ApiRevision {
    type Error = ApiTableError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            5 => Ok(Self::V5),
            6 => Ok(Self::V6),
            requested => Err(ApiTableError::UnsupportedRevision { requested }),
        }
    }
}

/// Size and alignment of one ABI table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiTableLayout {
    size: usize,
    align: usize,
}

impl ApiTableLayout {
    const fn of<T>() -> Self {
        Self {
            size: size_of::<T>(),
            align: align_of::<T>(),
        }
    }

    /// Returns the table size in bytes.
    #[must_use]
    pub const fn size(self) -> usize {
        self.size
    }

    /// Returns the table alignment in bytes.
    #[must_use]
    pub const fn align(self) -> usize {
        self.align
    }
}

/// Failure to select or allocate an API table.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ApiTableError {
    /// The add-on requested a revision outside the supported 1–6 range.
    #[error("unsupported add-on API revision {requested}; supported revisions are 1 through 6")]
    UnsupportedRevision {
        /// Raw revision requested by the add-on.
        requested: u32,
    },
    /// The current target cannot provide the alignment required by the ABI.
    #[error(
        "API revision {revision:?} requires alignment {required}, but word storage provides {available}"
    )]
    UnsupportedAlignment {
        /// Revision that could not be represented.
        revision: ApiRevision,
        /// ABI-required alignment.
        required: usize,
        /// Alignment provided by the backing word allocation.
        available: usize,
    },
    /// An ABI table was not an integral number of native words.
    #[error("API revision {revision:?} has byte size {size}, which is not word aligned")]
    NonIntegralWordLayout {
        /// Revision with the invalid layout.
        revision: ApiRevision,
        /// Table byte size.
        size: usize,
    },
}

/// One stable, host-owned allocation for each API revision.
///
/// The allocations begin zeroed and are deliberately opaque. A later API
/// assembly layer may populate them before any native load callback is invoked.
pub struct ApiTableCatalog {
    tables: [PinnedApiTable; 6],
    populated: bool,
}

/// Fully assembled typed values for every supported add-on API revision.
///
/// The assembly layer constructs this only after every service pointer has a
/// process-lifetime implementation. [`ApiTableCatalog::from_tables`] copies the
/// values into pinned, word-aligned storage before any native callback observes
/// them.
pub struct ApiTables {
    /// Flat API revision 1.
    pub v1: AddonApiV1,
    /// Flat API revision 2.
    pub v2: AddonApiV2,
    /// Flat API revision 3.
    pub v3: AddonApiV3,
    /// Flat API revision 4.
    pub v4: AddonApiV4,
    /// Flat API revision 5.
    pub v5: AddonApiV5,
    /// Grouped API revision 6.
    pub v6: AddonApiV6,
}

impl ApiTableCatalog {
    /// Allocates exactly one pinned table for every supported revision.
    pub fn new() -> Result<Self, ApiTableError> {
        Ok(Self {
            tables: [
                PinnedApiTable::new(ApiRevision::V1)?,
                PinnedApiTable::new(ApiRevision::V2)?,
                PinnedApiTable::new(ApiRevision::V3)?,
                PinnedApiTable::new(ApiRevision::V4)?,
                PinnedApiTable::new(ApiRevision::V5)?,
                PinnedApiTable::new(ApiRevision::V6)?,
            ],
            populated: false,
        })
    }

    /// Copies fully assembled typed API revisions into stable pinned storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot represent one of the ABI layouts.
    pub fn from_tables(tables: ApiTables) -> Result<Self, ApiTableError> {
        Ok(Self {
            tables: [
                PinnedApiTable::from_value(ApiRevision::V1, &tables.v1)?,
                PinnedApiTable::from_value(ApiRevision::V2, &tables.v2)?,
                PinnedApiTable::from_value(ApiRevision::V3, &tables.v3)?,
                PinnedApiTable::from_value(ApiRevision::V4, &tables.v4)?,
                PinnedApiTable::from_value(ApiRevision::V5, &tables.v5)?,
                PinnedApiTable::from_value(ApiRevision::V6, &tables.v6)?,
            ],
            populated: true,
        })
    }

    /// Returns whether every table came from an explicit typed assembly.
    #[must_use]
    pub const fn is_populated(&self) -> bool {
        self.populated
    }

    /// Borrows the stable allocation for `revision`.
    #[must_use]
    pub fn get(&self, revision: ApiRevision) -> ApiTableRef<'_> {
        self.tables[revision.index()].as_ref()
    }
}

impl Default for ApiTableCatalog {
    fn default() -> Self {
        Self::new().expect("the Nexus x64 ABI tables fit native word storage")
    }
}

struct PinnedApiTable {
    revision: ApiRevision,
    storage: Pin<Box<[usize]>>,
}

impl PinnedApiTable {
    fn new(revision: ApiRevision) -> Result<Self, ApiTableError> {
        let layout = revision.layout();
        let available_alignment = align_of::<usize>();
        if layout.align > available_alignment {
            return Err(ApiTableError::UnsupportedAlignment {
                revision,
                required: layout.align,
                available: available_alignment,
            });
        }
        if !layout.size.is_multiple_of(size_of::<usize>()) {
            return Err(ApiTableError::NonIntegralWordLayout {
                revision,
                size: layout.size,
            });
        }

        let words = vec![0; layout.size / size_of::<usize>()].into_boxed_slice();
        Ok(Self {
            revision,
            storage: Box::into_pin(words),
        })
    }

    fn from_value<T: Copy>(revision: ApiRevision, value: &T) -> Result<Self, ApiTableError> {
        let mut table = Self::new(revision)?;
        debug_assert_eq!(revision.layout().size(), size_of::<T>());
        debug_assert_eq!(revision.layout().align(), align_of::<T>());
        let destination = table.storage.as_mut().get_mut().as_mut_ptr().cast::<u8>();
        let source = from_ref(value).cast::<u8>();
        // SAFETY: `new` allocated exactly the revision's byte size with at
        // least its required alignment. Each call pairs a revision with its
        // exact repr(C), Copy-only ABI table type; the regions cannot overlap.
        // The destination remains opaque bytes and is never dropped as `T`.
        unsafe { copy_nonoverlapping(source, destination, size_of::<T>()) };
        Ok(table)
    }

    fn as_ref(&self) -> ApiTableRef<'_> {
        ApiTableRef { table: self }
    }
}

/// A lifetime-bound view of a host-owned API table allocation.
#[derive(Clone, Copy)]
pub struct ApiTableRef<'a> {
    table: &'a PinnedApiTable,
}

impl ApiTableRef<'_> {
    /// Returns the represented API revision.
    #[must_use]
    pub const fn revision(self) -> ApiRevision {
        self.table.revision
    }

    /// Returns the table's ABI layout.
    #[must_use]
    pub const fn layout(self) -> ApiTableLayout {
        self.table.revision.layout()
    }

    /// Returns a non-owning opaque ABI address.
    ///
    /// The address is stable for the lifetime of the catalog. The table is not
    /// callable until a later assembly layer has populated every required slot.
    #[must_use]
    pub fn as_opaque_ptr(self) -> NonNull<AddonApi> {
        NonNull::from(&self.table.storage[0]).cast()
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::c_void, mem::align_of};

    use nexus_abi::{AddonApiV1, AddonApiV2, AddonApiV3, AddonApiV4, AddonApiV5, AddonApiV6};

    use super::{ApiRevision, ApiTableCatalog, ApiTables};

    fn empty_tables() -> ApiTables {
        // SAFETY: every field in these repr(C) ABI tables is a raw pointer,
        // nullable function pointer, or a nested table of the same. The all-zero
        // representation is therefore the intentionally empty table state.
        unsafe {
            ApiTables {
                v1: std::mem::zeroed::<AddonApiV1>(),
                v2: std::mem::zeroed::<AddonApiV2>(),
                v3: std::mem::zeroed::<AddonApiV3>(),
                v4: std::mem::zeroed::<AddonApiV4>(),
                v5: std::mem::zeroed::<AddonApiV5>(),
                v6: std::mem::zeroed::<AddonApiV6>(),
            }
        }
    }

    #[test]
    fn all_revision_layouts_match_the_x64_abi() {
        let expected_sizes = [32 * 8, 42 * 8, 45 * 8, 51 * 8, 53 * 8, 62 * 8];

        for (revision, expected_size) in ApiRevision::ALL.into_iter().zip(expected_sizes) {
            assert_eq!(revision.layout().size(), expected_size);
            assert_eq!(revision.layout().align(), 8);
        }
    }

    #[test]
    fn each_revision_has_one_stable_zeroed_allocation() {
        let catalog = ApiTableCatalog::new().expect("x64 layouts should allocate");
        assert!(!catalog.is_populated());

        for revision in ApiRevision::ALL {
            let first = catalog.get(revision);
            let second = catalog.get(revision);
            assert_eq!(first.as_opaque_ptr(), second.as_opaque_ptr());
            assert_eq!(
                first
                    .as_opaque_ptr()
                    .as_ptr()
                    .align_offset(align_of::<usize>()),
                0
            );
        }

        for pair in ApiRevision::ALL.windows(2) {
            assert_ne!(
                catalog.get(pair[0]).as_opaque_ptr(),
                catalog.get(pair[1]).as_opaque_ptr()
            );
        }
    }

    #[test]
    fn typed_tables_are_copied_into_pinned_storage() {
        let mut tables = empty_tables();
        let marker = 0x1234_usize as *mut c_void;
        tables.v1.swap_chain = marker;
        tables.v6.imgui_context = marker;
        let catalog = ApiTableCatalog::from_tables(tables).expect("x64 layouts should copy");

        assert!(catalog.is_populated());
        // SAFETY: `from_tables` copied the exact typed revision into its pinned
        // allocation, which remains live for this borrow.
        let v1 = unsafe {
            catalog
                .get(ApiRevision::V1)
                .as_opaque_ptr()
                .cast::<AddonApiV1>()
                .as_ref()
        };
        // SAFETY: same argument for revision 6.
        let v6 = unsafe {
            catalog
                .get(ApiRevision::V6)
                .as_opaque_ptr()
                .cast::<AddonApiV6>()
                .as_ref()
        };
        assert_eq!(v1.swap_chain, marker);
        assert_eq!(v6.imgui_context, marker);
    }

    #[test]
    fn rejects_unknown_revisions() {
        assert!(ApiRevision::try_from(0).is_err());
        assert!(ApiRevision::try_from(7).is_err());
    }
}
