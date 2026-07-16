use std::ffi::c_void;
use std::fmt;
use std::mem::{MaybeUninit, align_of, size_of};
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::slice;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::{AddRef, ComInterfaceLayout, ComMethod, Release, ReleaseFn, VtableError};

type ErasedVtableEntry = *const c_void;

/// A prepared, owned vtable copy for one COM interface object.
///
/// Replacements can be configured while this value is detached. Calling
/// [`install`](Self::install) publishes the immutable shadow and returns an
/// [`InstalledVtable`] guard.
pub struct VtableShadow<L: ComInterfaceLayout> {
    target: NonNull<c_void>,
    original_vtable: NonNull<ErasedVtableEntry>,
    original_entries: Box<[ErasedVtableEntry]>,
    shadow_entries: Box<[ErasedVtableEntry]>,
    _layout: std::marker::PhantomData<L>,
}

impl<L: ComInterfaceLayout> fmt::Debug for VtableShadow<L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VtableShadow")
            .field("layout", &L::NAME)
            .field("slot_count", &L::SLOT_COUNT)
            .finish_non_exhaustive()
    }
}

impl<L: ComInterfaceLayout> VtableShadow<L> {
    /// Copies the complete vtable currently published by `interface`.
    ///
    /// # Errors
    ///
    /// Returns a [`VtableError`] for a null or misaligned pointer, an invalid
    /// layout, a null vtable, or a null vtable entry.
    ///
    /// # Safety
    ///
    /// The non-null pointer must identify a live, writable COM interface whose
    /// first machine word is a pointer to at least `L::SLOT_COUNT` readable
    /// entries matching `L`. The caller must keep its existing COM reference
    /// alive until this shadow is installed or discarded. Any earlier shadow
    /// that owns the observed vtable must outlive this value and an installed
    /// guard derived from it.
    pub unsafe fn copy_from(interface: *mut c_void) -> Result<Self, VtableError> {
        validate_layout::<L>()?;

        let target = NonNull::new(interface).ok_or(VtableError::NullInterface)?;
        if !(target.as_ptr() as usize).is_multiple_of(align_of::<*mut c_void>()) {
            return Err(VtableError::MisalignedInterface { layout: L::NAME });
        }

        // SAFETY: the caller promises that the first aligned word is a live,
        // writable COM vtable pointer for the duration of this operation.
        let original_pointer = unsafe { vtable_cell(target) }.load(Ordering::Acquire);
        let original_vtable =
            NonNull::new(original_pointer).ok_or(VtableError::NullVtable { layout: L::NAME })?;
        if !(original_vtable.as_ptr() as usize).is_multiple_of(align_of::<ErasedVtableEntry>()) {
            return Err(VtableError::MisalignedVtable { layout: L::NAME });
        }

        // SAFETY: layout validation bounds the byte length, and the caller
        // promises that the vtable exposes this many readable entries.
        let source =
            unsafe { slice::from_raw_parts(original_vtable.as_ptr().cast_const(), L::SLOT_COUNT) };
        for (index, entry) in source.iter().enumerate() {
            if entry.is_null() {
                return Err(VtableError::NullEntry {
                    layout: L::NAME,
                    index,
                });
            }
        }

        let original_entries = source.to_vec().into_boxed_slice();
        let shadow_entries = original_entries.clone();
        Ok(Self {
            target,
            original_vtable,
            original_entries,
            shadow_entries,
            _layout: std::marker::PhantomData,
        })
    }

    /// Returns the typed original function pointer for `M`.
    pub fn original<M>(&self) -> Result<M::Function, VtableError>
    where
        M: ComMethod<L>,
    {
        let index = validate_method::<L, M>()?;
        decode_method::<L, M>(self.original_entries[index])
    }

    /// Returns the typed function pointer currently prepared for `M`.
    pub fn published<M>(&self) -> Result<M::Function, VtableError>
    where
        M: ComMethod<L>,
    {
        let index = validate_method::<L, M>()?;
        decode_method::<L, M>(self.shadow_entries[index])
    }

    /// Replaces `M` in the detached shadow and returns the previous function.
    pub fn replace<M>(&mut self, replacement: M::Function) -> Result<M::Function, VtableError>
    where
        M: ComMethod<L>,
    {
        let index = validate_method::<L, M>()?;
        let previous = decode_method::<L, M>(self.shadow_entries[index])?;
        self.shadow_entries[index] = encode_method::<L, M>(replacement)?;
        Ok(previous)
    }

    /// Returns the interface object this per-instance shadow was copied from.
    pub const fn interface(&self) -> NonNull<c_void> {
        self.target
    }

    /// Publishes the prepared shadow and holds an extra COM reference.
    ///
    /// Installation uses a compare-exchange, so it never overwrites a vtable
    /// installed after [`copy_from`](Self::copy_from).
    ///
    /// # Errors
    ///
    /// Returns [`VtableError::VtableChanged`] if the object's vtable changed,
    /// or a method representation error if the layout contract is invalid.
    ///
    /// # Safety
    ///
    /// The `copy_from` obligations must still hold. The caller must keep the
    /// returned guard alive while any thread can read its shadow, and must
    /// quiesce interface method dispatch before the guard is dropped. This is
    /// necessary because a dispatcher can cache the shadow pointer before a
    /// restore. The guard is deliberately neither `Send` nor `Sync` so COM
    /// apartment policy remains explicit at a higher layer.
    pub unsafe fn install(self) -> Result<InstalledVtable<L>, VtableError> {
        let add_ref = self.original::<AddRef>()?;
        let release = self.original::<Release>()?;
        let original_pointer = self.original_vtable.as_ptr();
        let shadow_pointer = self.shadow_entries.as_ptr().cast_mut();

        // SAFETY: the copy_from contract guarantees a live IUnknown layout.
        unsafe { add_ref(self.target.as_ptr()) };

        // SAFETY: copy_from validated the aligned, writable first object word.
        let exchange = unsafe { vtable_cell(self.target) }.compare_exchange(
            original_pointer,
            shadow_pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if exchange.is_err() {
            // SAFETY: this balances the successful AddRef above using the same
            // original IUnknown implementation; the shadow was not published.
            unsafe { release(self.target.as_ptr()) };
            return Err(VtableError::VtableChanged { layout: L::NAME });
        }

        Ok(InstalledVtable {
            shadow: self,
            release,
            state: InstallState::Installed,
            _apartment: std::marker::PhantomData,
        })
    }
}

/// Current publication state of an [`InstalledVtable`] guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallState {
    /// The object's first word still points at the owned shadow.
    Installed,
    /// The original vtable has been restored.
    Restored,
    /// Another vtable replaced the shadow before restoration.
    Displaced,
}

/// RAII guard for one installed, per-instance COM shadow vtable.
///
/// Dropping an installed guard attempts to restore the original pointer and
/// always balances the COM reference acquired during installation. See the
/// quiescence requirement on [`VtableShadow::install`].
pub struct InstalledVtable<L: ComInterfaceLayout> {
    shadow: VtableShadow<L>,
    release: ReleaseFn,
    state: InstallState,
    _apartment: std::marker::PhantomData<Rc<()>>,
}

impl<L: ComInterfaceLayout> fmt::Debug for InstalledVtable<L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstalledVtable")
            .field("layout", &L::NAME)
            .field("slot_count", &L::SLOT_COUNT)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<L: ComInterfaceLayout> InstalledVtable<L> {
    /// Returns the guard's publication state.
    pub const fn state(&self) -> InstallState {
        self.state
    }

    /// Returns the hooked COM interface pointer.
    pub const fn interface(&self) -> NonNull<c_void> {
        self.shadow.interface()
    }

    /// Returns the typed function from the original vtable copy.
    pub fn original<M>(&self) -> Result<M::Function, VtableError>
    where
        M: ComMethod<L>,
    {
        self.shadow.original::<M>()
    }

    /// Returns the typed function published by the shadow.
    pub fn published<M>(&self) -> Result<M::Function, VtableError>
    where
        M: ComMethod<L>,
    {
        self.shadow.published::<M>()
    }

    /// Restores the original vtable without freeing the shadow storage.
    ///
    /// Keeping the guard alive after this call lets higher-level code wait for
    /// in-flight detours before dropping the owned shadow. If another component
    /// displaced this shadow, its vtable is left untouched and
    /// [`VtableError::VtableDisplaced`] is returned. This method balances the
    /// guard's extra COM reference exactly once in every outcome.
    pub fn restore(&mut self) -> Result<(), VtableError> {
        match self.state {
            InstallState::Restored => return Ok(()),
            InstallState::Displaced => {
                return Err(VtableError::VtableDisplaced { layout: L::NAME });
            }
            InstallState::Installed => {}
        }

        let original_pointer = self.shadow.original_vtable.as_ptr();
        let shadow_pointer = self.shadow.shadow_entries.as_ptr().cast_mut();
        // SAFETY: installation holds a COM reference and copy_from validated
        // the aligned first word. Storage remains owned after this operation.
        let exchange = unsafe { vtable_cell(self.shadow.target) }.compare_exchange(
            shadow_pointer,
            original_pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        let next_state = match exchange {
            Ok(_) => InstallState::Restored,
            Err(current) if ptr::eq(current, original_pointer) => InstallState::Restored,
            Err(_) => InstallState::Displaced,
        };
        self.state = next_state;

        // SAFETY: install acquired this exact reference through the original
        // implementation, and state prevents a second balancing call.
        unsafe { (self.release)(self.shadow.target.as_ptr()) };

        match next_state {
            InstallState::Displaced => Err(VtableError::VtableDisplaced { layout: L::NAME }),
            InstallState::Installed | InstallState::Restored => Ok(()),
        }
    }
}

impl<L: ComInterfaceLayout> Drop for InstalledVtable<L> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn validate_layout<L: ComInterfaceLayout>() -> Result<(), VtableError> {
    let valid_size = L::SLOT_COUNT
        .checked_mul(size_of::<ErasedVtableEntry>())
        .is_some_and(|bytes| bytes <= isize::MAX as usize);
    if L::SLOT_COUNT < 3 || !valid_size {
        return Err(VtableError::InvalidLayout {
            layout: L::NAME,
            slot_count: L::SLOT_COUNT,
        });
    }
    Ok(())
}

fn validate_method<L, M>() -> Result<usize, VtableError>
where
    L: ComInterfaceLayout,
    M: ComMethod<L>,
{
    if M::INDEX >= L::SLOT_COUNT {
        return Err(VtableError::SlotOutOfBounds {
            layout: L::NAME,
            method: M::NAME,
            index: M::INDEX,
            slot_count: L::SLOT_COUNT,
        });
    }
    if size_of::<M::Function>() != size_of::<ErasedVtableEntry>() {
        return Err(VtableError::InvalidMethodRepresentation {
            method: M::NAME,
            actual_size: size_of::<M::Function>(),
        });
    }
    Ok(M::INDEX)
}

fn decode_method<L, M>(entry: ErasedVtableEntry) -> Result<M::Function, VtableError>
where
    L: ComInterfaceLayout,
    M: ComMethod<L>,
{
    validate_method::<L, M>()?;
    let mut function = MaybeUninit::<M::Function>::uninit();
    // SAFETY: ComMethod promises that Function is a pointer-sized function
    // pointer, validated above, and copy_from rejected null entry bit patterns.
    unsafe {
        ptr::copy_nonoverlapping(
            ptr::from_ref(&entry).cast::<u8>(),
            function.as_mut_ptr().cast::<u8>(),
            size_of::<ErasedVtableEntry>(),
        );
        Ok(function.assume_init())
    }
}

fn encode_method<L, M>(function: M::Function) -> Result<ErasedVtableEntry, VtableError>
where
    L: ComInterfaceLayout,
    M: ComMethod<L>,
{
    validate_method::<L, M>()?;
    let mut entry = MaybeUninit::<ErasedVtableEntry>::uninit();
    // SAFETY: ComMethod promises the same pointer-sized representation checked
    // above, so copying its complete bytes into an erased pointer is valid.
    let entry = unsafe {
        ptr::copy_nonoverlapping(
            ptr::from_ref(&function).cast::<u8>(),
            entry.as_mut_ptr().cast::<u8>(),
            size_of::<ErasedVtableEntry>(),
        );
        entry.assume_init()
    };
    if entry.is_null() {
        return Err(VtableError::NullReplacement { method: M::NAME });
    }
    Ok(entry)
}

unsafe fn vtable_cell<'a>(target: NonNull<c_void>) -> &'a AtomicPtr<ErasedVtableEntry> {
    // SAFETY: callers establish that the target's aligned first word is valid
    // for atomic pointer access for the returned operation's lifetime.
    unsafe { AtomicPtr::from_ptr(target.as_ptr().cast::<*mut ErasedVtableEntry>()) }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::{QueryInterfaceFn, ReleaseFn};

    enum FakeLayout {}

    // SAFETY: FakeObject vtables in these tests contain four valid entries,
    // beginning with exact IUnknown-compatible functions.
    unsafe impl ComInterfaceLayout for FakeLayout {
        const NAME: &'static str = "FakeInterface";
        const SLOT_COUNT: usize = 4;
    }

    type ValueFn = unsafe extern "system" fn(this: *mut c_void) -> u32;

    struct Value;

    // SAFETY: FakeObject's fourth entry has exactly the ValueFn signature.
    unsafe impl ComMethod<FakeLayout> for Value {
        type Function = ValueFn;

        const INDEX: usize = 3;
        const NAME: &'static str = "FakeInterface::Value";
    }

    struct PastEnd;

    // SAFETY: this deliberately invalid index exercises runtime validation;
    // its function representation remains a valid pointer-sized method type.
    unsafe impl ComMethod<FakeLayout> for PastEnd {
        type Function = ValueFn;

        const INDEX: usize = 4;
        const NAME: &'static str = "FakeInterface::PastEnd";
    }

    #[repr(C)]
    struct FakeObject {
        vtable: *mut ErasedVtableEntry,
        references: Cell<u32>,
    }

    unsafe extern "system" fn fake_query_interface(
        _this: *mut c_void,
        _interface_id: *const c_void,
        _object: *mut *mut c_void,
    ) -> i32 {
        -2_147_467_262
    }

    unsafe extern "system" fn fake_add_ref(this: *mut c_void) -> u32 {
        // SAFETY: tests call through a FakeObject vtable with its own address.
        let object = unsafe { &*this.cast::<FakeObject>() };
        let references = object.references.get() + 1;
        object.references.set(references);
        references
    }

    unsafe extern "system" fn fake_release(this: *mut c_void) -> u32 {
        // SAFETY: tests call through a FakeObject vtable with its own address.
        let object = unsafe { &*this.cast::<FakeObject>() };
        let references = object.references.get() - 1;
        object.references.set(references);
        references
    }

    unsafe extern "system" fn original_value(_this: *mut c_void) -> u32 {
        7
    }

    unsafe extern "system" fn replacement_value(_this: *mut c_void) -> u32 {
        42
    }

    fn fake_vtable(value: ValueFn) -> Box<[ErasedVtableEntry]> {
        Box::new([
            fake_query_interface as QueryInterfaceFn as ErasedVtableEntry,
            fake_add_ref as crate::AddRefFn as ErasedVtableEntry,
            fake_release as ReleaseFn as ErasedVtableEntry,
            value as ErasedVtableEntry,
        ])
    }

    fn invoke_value(object: &FakeObject) -> u32 {
        // SAFETY: each test keeps the selected four-entry vtable alive and the
        // fourth entry has the ValueFn signature.
        unsafe {
            let entry = *object.vtable.add(3);
            let function = decode_method::<FakeLayout, Value>(entry)
                .expect("the fake value entry must decode");
            function(ptr::from_ref(object).cast_mut().cast())
        }
    }

    #[test]
    fn shadow_preserves_original_and_replaces_typed_method() {
        let mut table = fake_vtable(original_value);
        let mut object = FakeObject {
            vtable: table.as_mut_ptr(),
            references: Cell::new(1),
        };
        // SAFETY: object and its complete fake vtable outlive the shadow.
        let mut shadow =
            unsafe { VtableShadow::<FakeLayout>::copy_from(ptr::from_mut(&mut object).cast()) }
                .expect("the fake interface must be valid");

        let previous = shadow
            .replace::<Value>(replacement_value)
            .expect("the typed replacement must fit");

        // SAFETY: both functions accept this fake object's pointer.
        assert_eq!(unsafe { previous(ptr::from_mut(&mut object).cast()) }, 7);
        assert_eq!(
            shadow
                .original::<Value>()
                .expect("the original method must remain available") as usize,
            original_value as ValueFn as usize
        );
    }

    #[test]
    fn install_is_per_instance_and_drop_restores() {
        let mut table = fake_vtable(original_value);
        let mut first = FakeObject {
            vtable: table.as_mut_ptr(),
            references: Cell::new(1),
        };
        let second = FakeObject {
            vtable: table.as_mut_ptr(),
            references: Cell::new(1),
        };

        // SAFETY: both fake objects and their shared original vtable remain
        // live, and tests perform no concurrent method dispatch.
        let mut shadow =
            unsafe { VtableShadow::<FakeLayout>::copy_from(ptr::from_mut(&mut first).cast()) }
                .expect("the first fake interface must be valid");
        shadow
            .replace::<Value>(replacement_value)
            .expect("the replacement must fit");

        {
            // SAFETY: the test keeps the interface live and quiescent at drop.
            let guard = unsafe { shadow.install() }.expect("installation must succeed");
            assert_eq!(guard.state(), InstallState::Installed);
            assert_eq!(first.references.get(), 2);
            assert_eq!(invoke_value(&first), 42);
            assert_eq!(invoke_value(&second), 7);
        }

        assert!(ptr::eq(first.vtable, table.as_mut_ptr()));
        assert_eq!(first.references.get(), 1);
        assert_eq!(invoke_value(&first), 7);
    }

    #[test]
    fn explicit_restore_is_idempotent_and_keeps_storage_alive() {
        let mut table = fake_vtable(original_value);
        let mut object = FakeObject {
            vtable: table.as_mut_ptr(),
            references: Cell::new(1),
        };
        // SAFETY: the fake interface remains live and dispatch is quiescent.
        let shadow =
            unsafe { VtableShadow::<FakeLayout>::copy_from(ptr::from_mut(&mut object).cast()) }
                .expect("the fake interface must be valid");
        // SAFETY: the fake interface remains live and is quiescent at drop.
        let mut guard = unsafe { shadow.install() }.expect("installation must succeed");

        guard.restore().expect("restoration must succeed");
        guard
            .restore()
            .expect("a second restoration must be a no-op");

        assert_eq!(guard.state(), InstallState::Restored);
        assert!(ptr::eq(object.vtable, table.as_mut_ptr()));
        assert_eq!(object.references.get(), 1);
    }

    #[test]
    fn install_does_not_overwrite_a_changed_vtable() {
        let mut original = fake_vtable(original_value);
        let mut replacement = fake_vtable(replacement_value);
        let mut object = FakeObject {
            vtable: original.as_mut_ptr(),
            references: Cell::new(1),
        };
        // SAFETY: the original table remains live through the attempted install.
        let shadow =
            unsafe { VtableShadow::<FakeLayout>::copy_from(ptr::from_mut(&mut object).cast()) }
                .expect("the fake interface must be valid");
        object.vtable = replacement.as_mut_ptr();

        // SAFETY: both tables and the interface remain live and quiescent.
        let error =
            unsafe { shadow.install() }.expect_err("a changed vtable must reject installation");

        assert_eq!(
            error,
            VtableError::VtableChanged {
                layout: "FakeInterface"
            }
        );
        assert!(ptr::eq(object.vtable, replacement.as_mut_ptr()));
        assert_eq!(object.references.get(), 1);
    }

    #[test]
    fn typed_method_cannot_address_past_layout() {
        let mut table = fake_vtable(original_value);
        let mut object = FakeObject {
            vtable: table.as_mut_ptr(),
            references: Cell::new(1),
        };
        // SAFETY: object and table remain live for the detached shadow.
        let shadow =
            unsafe { VtableShadow::<FakeLayout>::copy_from(ptr::from_mut(&mut object).cast()) }
                .expect("the fake interface must be valid");

        assert_eq!(
            shadow.original::<PastEnd>(),
            Err(VtableError::SlotOutOfBounds {
                layout: "FakeInterface",
                method: "FakeInterface::PastEnd",
                index: 4,
                slot_count: 4,
            })
        );
    }
}
