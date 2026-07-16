//! Explicit COM reference ownership.

use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

use windows_sys::core::{GUID, HRESULT, IUnknown_Vtbl};

/// A failed HRESULT or an invalid successful COM result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HResultError {
    /// COM returned a failing HRESULT.
    Failure(HRESULT),
    /// COM returned success but did not initialize the requested interface.
    SuccessWithoutObject(HRESULT),
}

impl HResultError {
    /// The exact HRESULT returned by COM.
    #[must_use]
    pub const fn code(self) -> HRESULT {
        match self {
            Self::Failure(code) | Self::SuccessWithoutObject(code) => code,
        }
    }
}

impl fmt::Display for HResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failure(code) => write!(formatter, "COM failed with HRESULT {code:#010x}"),
            Self::SuccessWithoutObject(code) => write!(
                formatter,
                "COM returned HRESULT {code:#010x} without an interface pointer"
            ),
        }
    }
}

impl std::error::Error for HResultError {}

/// One owned COM interface reference.
///
/// Cloning calls `IUnknown::AddRef`; dropping calls `IUnknown::Release`.
/// The handle is intentionally thread-bound because captured immediate-context
/// state is restored on its capture thread.
pub struct OwnedComObject {
    pointer: NonNull<c_void>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl OwnedComObject {
    /// Take ownership of one existing COM reference.
    ///
    /// Returns `None` for a null pointer.
    ///
    /// # Safety
    ///
    /// A non-null `pointer` must identify a live COM interface whose first
    /// vtable entries have the `IUnknown` ABI. The caller transfers exactly
    /// one owned reference to the returned value.
    pub unsafe fn from_raw_owned(pointer: *mut c_void) -> Option<Self> {
        NonNull::new(pointer).map(|pointer| Self {
            pointer,
            _thread_bound: PhantomData,
        })
    }

    /// Borrow a COM pointer by acquiring one owned reference.
    ///
    /// Returns `None` for a null pointer.
    ///
    /// # Safety
    ///
    /// A non-null `pointer` must identify a live COM interface whose first
    /// vtable entries have the `IUnknown` ABI for the duration of this call.
    pub unsafe fn from_raw_borrowed(pointer: *mut c_void) -> Option<Self> {
        // SAFETY: The caller guarantees a live COM interface and this method
        // immediately establishes the reference it returns.
        let object = unsafe { Self::from_raw_owned(pointer) }?;
        object.add_ref();
        Some(object)
    }

    /// Type-erased raw interface pointer.
    #[must_use]
    pub fn as_raw(&self) -> *mut c_void {
        self.pointer.as_ptr()
    }

    /// Query another interface and take ownership of the returned reference.
    ///
    /// # Errors
    ///
    /// Preserves the exact failing HRESULT, or reports a successful call that
    /// returned a null pointer.
    pub fn query_interface(&self, interface_id: &GUID) -> Result<Self, HResultError> {
        let mut result = std::ptr::null_mut();
        // SAFETY: `self` owns a live COM reference, the output points to valid
        // storage, and `interface_id` is borrowed for this call only.
        let result_code =
            unsafe { (self.vtable().QueryInterface)(self.as_raw(), interface_id, &mut result) };
        if result_code < 0 {
            return Err(HResultError::Failure(result_code));
        }
        // SAFETY: A successful QueryInterface returns one owned reference when
        // the output is non-null.
        unsafe { Self::from_raw_owned(result) }
            .ok_or(HResultError::SuccessWithoutObject(result_code))
    }

    fn add_ref(&self) {
        // SAFETY: `self` owns a live COM interface reference.
        unsafe {
            (self.vtable().AddRef)(self.as_raw());
        }
    }

    fn vtable(&self) -> &IUnknown_Vtbl {
        // SAFETY: Construction requires an IUnknown-compatible COM pointer,
        // whose first machine word is a non-null vtable pointer.
        unsafe { &**(self.as_raw().cast::<*const IUnknown_Vtbl>()) }
    }
}

impl Clone for OwnedComObject {
    fn clone(&self) -> Self {
        self.add_ref();
        Self {
            pointer: self.pointer,
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for OwnedComObject {
    fn drop(&mut self) {
        // SAFETY: `self` owns exactly one live COM interface reference.
        unsafe {
            (self.vtable().Release)(self.as_raw());
        }
    }
}

impl fmt::Debug for OwnedComObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("OwnedComObject")
            .field(&self.pointer)
            .finish()
    }
}

impl PartialEq for OwnedComObject {
    fn eq(&self, other: &Self) -> bool {
        self.pointer == other.pointer
    }
}

impl Eq for OwnedComObject {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const E_NOINTERFACE: HRESULT = -2_147_467_262;

    #[repr(C)]
    struct FakeUnknown {
        vtable: *const IUnknown_Vtbl,
        references: Cell<u32>,
        query_result: HRESULT,
    }

    unsafe extern "system" fn query_interface(
        this: *mut c_void,
        _interface_id: *const GUID,
        result: *mut *mut c_void,
    ) -> HRESULT {
        // SAFETY: Tests call through FAKE_VTABLE with a FakeUnknown pointer.
        let object = unsafe { &*(this.cast::<FakeUnknown>()) };
        if object.query_result < 0 {
            // SAFETY: COM supplies a valid output pointer in the test call.
            unsafe { result.write(std::ptr::null_mut()) };
            return object.query_result;
        }
        // SAFETY: COM supplies a valid output pointer in the test call.
        unsafe { result.write(this) };
        object.references.set(object.references.get() + 1);
        object.query_result
    }

    unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
        // SAFETY: Tests call through FAKE_VTABLE with a FakeUnknown pointer.
        let object = unsafe { &*(this.cast::<FakeUnknown>()) };
        let references = object.references.get() + 1;
        object.references.set(references);
        references
    }

    unsafe extern "system" fn release(this: *mut c_void) -> u32 {
        // SAFETY: Tests call through FAKE_VTABLE with a FakeUnknown pointer.
        let object = unsafe { &*(this.cast::<FakeUnknown>()) };
        let references = object.references.get() - 1;
        object.references.set(references);
        references
    }

    static FAKE_VTABLE: IUnknown_Vtbl = IUnknown_Vtbl {
        QueryInterface: query_interface,
        AddRef: add_ref,
        Release: release,
    };

    #[test]
    fn clone_and_drop_balance_add_ref_and_release() {
        let object = FakeUnknown {
            vtable: &FAKE_VTABLE,
            references: Cell::new(1),
            query_result: 0,
        };

        // SAFETY: `object` is a live fake COM object until all handles drop.
        let first = unsafe {
            OwnedComObject::from_raw_borrowed(
                std::ptr::from_ref(&object).cast_mut().cast::<c_void>(),
            )
        }
        .expect("the fake COM pointer is non-null");
        assert_eq!(object.references.get(), 2);
        let second = first.clone();
        assert_eq!(object.references.get(), 3);
        drop(second);
        assert_eq!(object.references.get(), 2);
        drop(first);
        assert_eq!(object.references.get(), 1);
    }

    #[test]
    fn query_interface_preserves_failing_hresult() {
        let object = FakeUnknown {
            vtable: &FAKE_VTABLE,
            references: Cell::new(1),
            query_result: E_NOINTERFACE,
        };
        // SAFETY: `object` is a live fake COM object until `owned` drops.
        let owned = unsafe {
            OwnedComObject::from_raw_borrowed(
                std::ptr::from_ref(&object).cast_mut().cast::<c_void>(),
            )
        }
        .expect("the fake COM pointer is non-null");

        let error = owned
            .query_interface(&windows_sys::core::IID_IUnknown)
            .expect_err("the fake QueryInterface call is configured to fail");
        assert_eq!(error, HResultError::Failure(E_NOINTERFACE));
        assert_eq!(error.code(), E_NOINTERFACE);
        assert_eq!(object.references.get(), 2);
    }

    #[test]
    fn successful_query_interface_owns_returned_reference() {
        let object = FakeUnknown {
            vtable: &FAKE_VTABLE,
            references: Cell::new(1),
            query_result: 0,
        };
        // SAFETY: `object` is a live fake COM object until both handles drop.
        let owned = unsafe {
            OwnedComObject::from_raw_borrowed(
                std::ptr::from_ref(&object).cast_mut().cast::<c_void>(),
            )
        }
        .expect("the fake COM pointer is non-null");
        let queried = owned
            .query_interface(&windows_sys::core::IID_IUnknown)
            .expect("the fake QueryInterface call succeeds");
        assert_eq!(object.references.get(), 3);
        drop(queried);
        assert_eq!(object.references.get(), 2);
    }
}
