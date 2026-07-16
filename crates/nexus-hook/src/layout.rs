use std::ffi::c_void;

/// Describes the complete vtable visible through one COM interface pointer.
///
/// # Safety
///
/// `SLOT_COUNT` must be the exact number of readable function-pointer entries
/// for the interface represented by `Self`. Entries zero through two must have
/// the standard `IUnknown` `QueryInterface`, `AddRef`, and `Release` ABIs.
pub unsafe trait ComInterfaceLayout: 'static {
    /// Human-readable interface name used in diagnostics.
    const NAME: &'static str;

    /// Number of function-pointer entries in the interface vtable.
    const SLOT_COUNT: usize;
}

/// Connects a named COM method to its interface layout, slot, and exact ABI.
///
/// Implementing this trait lets callers use
/// `shadow.original::<NamedMethod>()` instead of indexing a raw pointer array.
///
/// # Safety
///
/// `INDEX` must identify this method in `L`, and `Function` must be its exact,
/// non-null, pointer-sized function-pointer type, including the `system` ABI
/// and the leading `this` parameter.
pub unsafe trait ComMethod<L: ComInterfaceLayout>: 'static {
    /// Exact function-pointer type stored in the method's vtable entry.
    type Function: Copy;

    /// Zero-based vtable index.
    const INDEX: usize;

    /// Human-readable qualified method name used in diagnostics.
    const NAME: &'static str;
}

/// Raw `IUnknown::QueryInterface` function signature.
pub type QueryInterfaceFn = unsafe extern "system" fn(
    this: *mut c_void,
    interface_id: *const c_void,
    object: *mut *mut c_void,
) -> i32;

/// Raw `IUnknown::AddRef` function signature.
pub type AddRefFn = unsafe extern "system" fn(this: *mut c_void) -> u32;

/// Raw `IUnknown::Release` function signature.
pub type ReleaseFn = unsafe extern "system" fn(this: *mut c_void) -> u32;

/// Typed marker for `IUnknown::QueryInterface`.
pub struct QueryInterface;

/// Typed marker for `IUnknown::AddRef`.
pub struct AddRef;

/// Typed marker for `IUnknown::Release`.
pub struct Release;

// SAFETY: every ComInterfaceLayout promises the three leading IUnknown slots.
unsafe impl<L: ComInterfaceLayout> ComMethod<L> for QueryInterface {
    type Function = QueryInterfaceFn;

    const INDEX: usize = 0;
    const NAME: &'static str = "IUnknown::QueryInterface";
}

// SAFETY: every ComInterfaceLayout promises the three leading IUnknown slots.
unsafe impl<L: ComInterfaceLayout> ComMethod<L> for AddRef {
    type Function = AddRefFn;

    const INDEX: usize = 1;
    const NAME: &'static str = "IUnknown::AddRef";
}

// SAFETY: every ComInterfaceLayout promises the three leading IUnknown slots.
unsafe impl<L: ComInterfaceLayout> ComMethod<L> for Release {
    type Function = ReleaseFn;

    const INDEX: usize = 2;
    const NAME: &'static str = "IUnknown::Release";
}
