use std::{
    collections::HashMap,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};

use nexus_hook::{
    ComInterfaceLayout, ComMethod, InstallState, InstalledVtable, QueryInterface, QueryInterfaceFn,
    VtableError, VtableShadow,
    dxgi::{
        CreateSwapChain, CreateSwapChainFn, CreateSwapChainForComposition,
        CreateSwapChainForCompositionFn, CreateSwapChainForCompositionSurfaceHandle,
        CreateSwapChainForCompositionSurfaceHandleFn, CreateSwapChainForCoreWindow,
        CreateSwapChainForCoreWindowFn, CreateSwapChainForHwnd, CreateSwapChainForHwndFn,
        DxgiFactory, DxgiFactory1, DxgiFactory2, DxgiFactory3, DxgiFactory4, DxgiFactory5,
        DxgiFactory6, DxgiFactory7, DxgiFactoryMedia, DxgiSwapChain, DxgiSwapChain1,
        DxgiSwapChain2, DxgiSwapChain3, DxgiSwapChain4, Present, Present1, Present1Fn, PresentFn,
        ResizeBuffers, ResizeBuffers1, ResizeBuffers1Fn, ResizeBuffersFn, SetColorSpace1,
        SetColorSpace1Fn,
    },
};
use nexus_render::PresentMethod;
use windows::Win32::Foundation::E_FAIL;
use windows_sys::core::GUID;

use crate::{
    AttachOutcome, Boundary, DxgiError, DxgiObservationEvent, FactoryInterface, ObjectKind,
    SwapChainInterface, manager::Inner, sdk,
};

static ROUTES: OnceLock<Mutex<HashMap<usize, Route>>> = OnceLock::new();

#[derive(Clone)]
enum Route {
    Factory(FactoryRoute),
    FactoryMedia(FactoryMediaRoute),
    SwapChain(SwapChainRoute),
}

impl Route {
    const fn manager_id(&self) -> u64 {
        match self {
            Self::Factory(route) => route.manager_id,
            Self::FactoryMedia(route) => route.manager_id,
            Self::SwapChain(route) => route.manager_id,
        }
    }
}

#[derive(Clone)]
struct FactoryRoute {
    manager_id: u64,
    manager: Weak<Inner>,
    originals: FactoryOriginals,
}

#[derive(Clone)]
struct FactoryMediaRoute {
    manager_id: u64,
    manager: Weak<Inner>,
    originals: FactoryMediaOriginals,
}

#[derive(Clone)]
struct SwapChainRoute {
    manager_id: u64,
    manager: Weak<Inner>,
    originals: SwapChainOriginals,
}

#[derive(Clone, Copy)]
struct FactoryOriginals {
    query_interface: QueryInterfaceFn,
    create_swap_chain: CreateSwapChainFn,
    create_for_hwnd: Option<CreateSwapChainForHwndFn>,
    create_for_core_window: Option<CreateSwapChainForCoreWindowFn>,
    create_for_composition: Option<CreateSwapChainForCompositionFn>,
}

#[derive(Clone, Copy)]
struct FactoryMediaOriginals {
    query_interface: QueryInterfaceFn,
    create_for_composition_surface_handle: CreateSwapChainForCompositionSurfaceHandleFn,
}

#[derive(Clone, Copy)]
struct SwapChainOriginals {
    query_interface: QueryInterfaceFn,
    present: PresentFn,
    present1: Option<Present1Fn>,
    resize_buffers: ResizeBuffersFn,
    resize_buffers1: Option<ResizeBuffers1Fn>,
    set_color_space1: Option<SetColorSpace1Fn>,
}

/// Type-erased owner for one restored-or-installed shadow vtable.
pub(crate) enum HookGuard {
    Factory(ThreadSafeInstalled<DxgiFactory>),
    Factory1(ThreadSafeInstalled<DxgiFactory1>),
    Factory2(ThreadSafeInstalled<DxgiFactory2>),
    Factory3(ThreadSafeInstalled<DxgiFactory3>),
    Factory4(ThreadSafeInstalled<DxgiFactory4>),
    Factory5(ThreadSafeInstalled<DxgiFactory5>),
    Factory6(ThreadSafeInstalled<DxgiFactory6>),
    Factory7(ThreadSafeInstalled<DxgiFactory7>),
    FactoryMedia(ThreadSafeInstalled<DxgiFactoryMedia>),
    SwapChain(ThreadSafeInstalled<DxgiSwapChain>),
    SwapChain1(ThreadSafeInstalled<DxgiSwapChain1>),
    SwapChain2(ThreadSafeInstalled<DxgiSwapChain2>),
    SwapChain3(ThreadSafeInstalled<DxgiSwapChain3>),
    SwapChain4(ThreadSafeInstalled<DxgiSwapChain4>),
}

impl HookGuard {
    pub(crate) fn restore(&mut self) -> Result<bool, VtableError> {
        match self {
            Self::Factory(guard) => guard.restore(),
            Self::Factory1(guard) => guard.restore(),
            Self::Factory2(guard) => guard.restore(),
            Self::Factory3(guard) => guard.restore(),
            Self::Factory4(guard) => guard.restore(),
            Self::Factory5(guard) => guard.restore(),
            Self::Factory6(guard) => guard.restore(),
            Self::Factory7(guard) => guard.restore(),
            Self::FactoryMedia(guard) => guard.restore(),
            Self::SwapChain(guard) => guard.restore(),
            Self::SwapChain1(guard) => guard.restore(),
            Self::SwapChain2(guard) => guard.restore(),
            Self::SwapChain3(guard) => guard.restore(),
            Self::SwapChain4(guard) => guard.restore(),
        }
    }
}

pub(crate) struct ThreadSafeInstalled<L: ComInterfaceLayout>(InstalledVtable<L>);

// SAFETY: generated Windows SDK DXGI interface wrappers explicitly implement
// Send and Sync. DXGI factories and swap chains are free-threaded native
// objects, the shadow table is immutable after publication, pointer exchange
// is atomic inside nexus-hook, and every mutation/restoration of this guard is
// serialized by the manager's hooks mutex. CallbackGate supplies the required
// detour drain before the process-lifetime owner is released.
unsafe impl<L: ComInterfaceLayout> Send for ThreadSafeInstalled<L> {}

impl<L: ComInterfaceLayout> ThreadSafeInstalled<L> {
    fn restore(&mut self) -> Result<bool, VtableError> {
        let changed = self.0.state() == InstallState::Installed;
        self.0.restore()?;
        Ok(changed)
    }
}

pub(crate) unsafe fn attach_factory(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    iid: &GUID,
) -> Result<AttachOutcome, DxgiError> {
    let requested = sdk::factory_interface(iid).ok_or(DxgiError::UnsupportedInterface)?;
    validate_attachment(manager, pointer)?;
    if let Some(outcome) = existing_attachment(manager, pointer)? {
        return Ok(outcome);
    }

    // SAFETY: validation and the public attach contract guarantee IUnknown.
    let (query, _) = unsafe { sdk::original_iunknown_methods(pointer) }
        .ok_or(DxgiError::UnsupportedInterface)?;
    // SAFETY: this call only issues SDK QueryInterface requests and owns every result.
    let highest = unsafe { sdk::highest_factory(pointer, query) };
    let mut attached = 0_u32;

    match highest {
        Some((interface, queried)) if queried.pointer() == pointer => {
            // SAFETY: the successful SDK query proves the selected layout.
            unsafe { attach_factory_exact(manager, pointer, interface) }?;
            attached = 1;
        }
        Some((interface, queried)) => {
            // SAFETY: the caller's IID proves the original pointer layout.
            unsafe { attach_factory_exact(manager, pointer, requested) }?;
            attached = attached.saturating_add(1);
            // SAFETY: the owned SDK query proves the distinct derived layout.
            match unsafe { attach_factory_exact(manager, queried.pointer(), interface) } {
                Ok(()) => attached = attached.saturating_add(1),
                Err(DxgiError::HookConflict) => return Err(DxgiError::HookConflict),
                Err(error) => manager.report_attach_error(ObjectKind::Factory, None, &error),
            }
        }
        None => {
            // SAFETY: the caller's requested IID still proves this exact layout.
            unsafe { attach_factory_exact(manager, pointer, requested) }?;
            attached = 1;
        }
    }

    Ok(AttachOutcome::Attached {
        interfaces: attached,
    })
}

pub(crate) unsafe fn attach_swap_chain(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    iid: &GUID,
) -> Result<AttachOutcome, DxgiError> {
    let requested = sdk::swap_chain_interface(iid).ok_or(DxgiError::UnsupportedInterface)?;
    validate_attachment(manager, pointer)?;
    if let Some(outcome) = existing_attachment(manager, pointer)? {
        return Ok(outcome);
    }

    // SAFETY: validation and the public attach contract guarantee IUnknown.
    let (query, _) = unsafe { sdk::original_iunknown_methods(pointer) }
        .ok_or(DxgiError::UnsupportedInterface)?;
    // SAFETY: this call only issues SDK QueryInterface requests and owns every result.
    let highest = unsafe { sdk::highest_swap_chain(pointer, query) };
    let mut attached = 0_u32;

    match highest {
        Some((interface, queried)) if queried.pointer() == pointer => {
            // SAFETY: the successful SDK query proves the selected layout.
            unsafe { attach_swap_chain_exact(manager, pointer, interface) }?;
            attached = 1;
        }
        Some((interface, queried)) => {
            // SAFETY: the caller's IID proves the original pointer layout.
            unsafe { attach_swap_chain_exact(manager, pointer, requested) }?;
            attached = attached.saturating_add(1);
            // SAFETY: the owned SDK query proves the distinct derived layout.
            match unsafe { attach_swap_chain_exact(manager, queried.pointer(), interface) } {
                Ok(()) => attached = attached.saturating_add(1),
                Err(DxgiError::HookConflict) => return Err(DxgiError::HookConflict),
                Err(error) => {
                    let id = manager.track_swap_chain(pointer, requested);
                    manager.report_attach_error(ObjectKind::SwapChain, Some(id), &error);
                }
            }
        }
        None => {
            // SAFETY: the caller's requested IID still proves this exact layout.
            unsafe { attach_swap_chain_exact(manager, pointer, requested) }?;
            attached = 1;
        }
    }

    Ok(AttachOutcome::Attached {
        interfaces: attached,
    })
}

fn validate_attachment(manager: &Inner, pointer: *mut c_void) -> Result<(), DxgiError> {
    if pointer.is_null() {
        return Err(DxgiError::NullInterface);
    }
    if manager.is_closing() {
        return Err(DxgiError::ManagerClosed);
    }
    Ok(())
}

fn existing_attachment(
    manager: &Inner,
    pointer: *mut c_void,
) -> Result<Option<AttachOutcome>, DxgiError> {
    let routes = lock(routes());
    let Some(route) = routes.get(&(pointer as usize)) else {
        return Ok(None);
    };
    if route.manager_id() == manager.id {
        Ok(Some(AttachOutcome::AlreadyAttached))
    } else {
        Err(DxgiError::HookConflict)
    }
}

unsafe fn attach_factory_exact(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    interface: FactoryInterface,
) -> Result<(), DxgiError> {
    match interface {
        FactoryInterface::Base => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory_base::<DxgiFactory>(manager, pointer, HookGuard::Factory) }
        }
        FactoryInterface::V1 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory_base::<DxgiFactory1>(manager, pointer, HookGuard::Factory1) }
        }
        FactoryInterface::V2 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory2>(manager, pointer, HookGuard::Factory2) }
        }
        FactoryInterface::V3 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory3>(manager, pointer, HookGuard::Factory3) }
        }
        FactoryInterface::V4 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory4>(manager, pointer, HookGuard::Factory4) }
        }
        FactoryInterface::V5 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory5>(manager, pointer, HookGuard::Factory5) }
        }
        FactoryInterface::V6 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory6>(manager, pointer, HookGuard::Factory6) }
        }
        FactoryInterface::V7 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe { install_factory2::<DxgiFactory7>(manager, pointer, HookGuard::Factory7) }
        }
        FactoryInterface::Media => {
            // SAFETY: interface classification proves the exact independent layout.
            unsafe {
                install_factory_media::<DxgiFactoryMedia>(manager, pointer, HookGuard::FactoryMedia)
            }
        }
    }
    .map(|()| {
        manager.emit_observation(DxgiObservationEvent::FactoryAttached { interface });
    })
}

unsafe fn attach_swap_chain_exact(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    interface: SwapChainInterface,
) -> Result<(), DxgiError> {
    let result = match interface {
        SwapChainInterface::Base => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe {
                install_swap_chain_base::<DxgiSwapChain>(manager, pointer, HookGuard::SwapChain)
            }
        }
        SwapChainInterface::V1 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe {
                install_swap_chain1::<DxgiSwapChain1>(manager, pointer, HookGuard::SwapChain1)
            }
        }
        SwapChainInterface::V2 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe {
                install_swap_chain1::<DxgiSwapChain2>(manager, pointer, HookGuard::SwapChain2)
            }
        }
        SwapChainInterface::V3 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe {
                install_swap_chain3::<DxgiSwapChain3>(manager, pointer, HookGuard::SwapChain3)
            }
        }
        SwapChainInterface::V4 => {
            // SAFETY: interface classification proves the exact inherited layout.
            unsafe {
                install_swap_chain3::<DxgiSwapChain4>(manager, pointer, HookGuard::SwapChain4)
            }
        }
    };
    result.map(|()| {
        let _ = manager.track_swap_chain(pointer, interface);
    })
}

unsafe fn install_factory_base<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    CreateSwapChain: ComMethod<L, Function = CreateSwapChainFn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = FactoryOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        create_swap_chain: shadow.original::<CreateSwapChain>()?,
        create_for_hwnd: None,
        create_for_core_window: None,
        create_for_composition: None,
    };
    shadow.replace::<QueryInterface>(factory_query_interface_detour)?;
    shadow.replace::<CreateSwapChain>(factory_create_swap_chain_detour)?;
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe { publish_factory(manager, pointer, shadow, originals, wrap) }
}

unsafe fn install_factory2<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    CreateSwapChain: ComMethod<L, Function = CreateSwapChainFn>,
    CreateSwapChainForHwnd: ComMethod<L, Function = CreateSwapChainForHwndFn>,
    CreateSwapChainForCoreWindow: ComMethod<L, Function = CreateSwapChainForCoreWindowFn>,
    CreateSwapChainForComposition: ComMethod<L, Function = CreateSwapChainForCompositionFn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = FactoryOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        create_swap_chain: shadow.original::<CreateSwapChain>()?,
        create_for_hwnd: Some(shadow.original::<CreateSwapChainForHwnd>()?),
        create_for_core_window: Some(shadow.original::<CreateSwapChainForCoreWindow>()?),
        create_for_composition: Some(shadow.original::<CreateSwapChainForComposition>()?),
    };
    shadow.replace::<QueryInterface>(factory_query_interface_detour)?;
    shadow.replace::<CreateSwapChain>(factory_create_swap_chain_detour)?;
    shadow.replace::<CreateSwapChainForHwnd>(factory_create_swap_chain_for_hwnd_detour)?;
    shadow.replace::<CreateSwapChainForCoreWindow>(
        factory_create_swap_chain_for_core_window_detour,
    )?;
    shadow.replace::<CreateSwapChainForComposition>(
        factory_create_swap_chain_for_composition_detour,
    )?;
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe { publish_factory(manager, pointer, shadow, originals, wrap) }
}

unsafe fn install_factory_media<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    CreateSwapChainForCompositionSurfaceHandle:
        ComMethod<L, Function = CreateSwapChainForCompositionSurfaceHandleFn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = FactoryMediaOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        create_for_composition_surface_handle: shadow
            .original::<CreateSwapChainForCompositionSurfaceHandle>()?,
    };
    shadow.replace::<QueryInterface>(factory_query_interface_detour)?;
    shadow.replace::<CreateSwapChainForCompositionSurfaceHandle>(
        factory_create_swap_chain_for_composition_surface_handle_detour,
    )?;
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe { publish_factory_media(manager, pointer, shadow, originals, wrap) }
}

unsafe fn publish_factory<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    shadow: VtableShadow<L>,
    originals: FactoryOriginals,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
{
    let route = Route::Factory(FactoryRoute {
        manager_id: manager.id,
        manager: Arc::downgrade(manager),
        originals,
    });
    register_route(pointer, &route)?;
    // SAFETY: the caller upholds VtableShadow's lifetime and matching-layout contract.
    let installed = match unsafe { shadow.install() } {
        Ok(installed) => installed,
        Err(error) => {
            unregister_route(manager.id, pointer);
            return Err(error.into());
        }
    };
    lock(&manager.hooks).push(wrap(ThreadSafeInstalled(installed)));
    Ok(())
}

unsafe fn publish_factory_media<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    shadow: VtableShadow<L>,
    originals: FactoryMediaOriginals,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
{
    let route = Route::FactoryMedia(FactoryMediaRoute {
        manager_id: manager.id,
        manager: Arc::downgrade(manager),
        originals,
    });
    register_route(pointer, &route)?;
    // SAFETY: the caller upholds VtableShadow's lifetime and matching-layout contract.
    let installed = match unsafe { shadow.install() } {
        Ok(installed) => installed,
        Err(error) => {
            unregister_route(manager.id, pointer);
            return Err(error.into());
        }
    };
    lock(&manager.hooks).push(wrap(ThreadSafeInstalled(installed)));
    Ok(())
}

unsafe fn install_swap_chain_base<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    Present: ComMethod<L, Function = PresentFn>,
    ResizeBuffers: ComMethod<L, Function = ResizeBuffersFn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = SwapChainOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        present: shadow.original::<Present>()?,
        present1: None,
        resize_buffers: shadow.original::<ResizeBuffers>()?,
        resize_buffers1: None,
        set_color_space1: None,
    };
    shadow.replace::<QueryInterface>(swap_chain_query_interface_detour)?;
    shadow.replace::<Present>(present_detour)?;
    shadow.replace::<ResizeBuffers>(resize_buffers_detour)?;
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe {
        publish_swap_chain(
            manager,
            pointer,
            SwapChainInterface::Base,
            shadow,
            originals,
            wrap,
        )
    }
}

unsafe fn install_swap_chain1<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    Present: ComMethod<L, Function = PresentFn>,
    Present1: ComMethod<L, Function = Present1Fn>,
    ResizeBuffers: ComMethod<L, Function = ResizeBuffersFn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = SwapChainOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        present: shadow.original::<Present>()?,
        present1: Some(shadow.original::<Present1>()?),
        resize_buffers: shadow.original::<ResizeBuffers>()?,
        resize_buffers1: None,
        set_color_space1: None,
    };
    shadow.replace::<QueryInterface>(swap_chain_query_interface_detour)?;
    shadow.replace::<Present>(present_detour)?;
    shadow.replace::<Present1>(present1_detour)?;
    shadow.replace::<ResizeBuffers>(resize_buffers_detour)?;
    let interface = if L::SLOT_COUNT == <DxgiSwapChain1 as ComInterfaceLayout>::SLOT_COUNT {
        SwapChainInterface::V1
    } else {
        SwapChainInterface::V2
    };
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe { publish_swap_chain(manager, pointer, interface, shadow, originals, wrap) }
}

unsafe fn install_swap_chain3<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
    QueryInterface: ComMethod<L, Function = QueryInterfaceFn>,
    Present: ComMethod<L, Function = PresentFn>,
    Present1: ComMethod<L, Function = Present1Fn>,
    ResizeBuffers: ComMethod<L, Function = ResizeBuffersFn>,
    SetColorSpace1: ComMethod<L, Function = SetColorSpace1Fn>,
    ResizeBuffers1: ComMethod<L, Function = ResizeBuffers1Fn>,
{
    // SAFETY: the caller proves the concrete interface matches L.
    let mut shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }?;
    let originals = SwapChainOriginals {
        query_interface: shadow.original::<QueryInterface>()?,
        present: shadow.original::<Present>()?,
        present1: Some(shadow.original::<Present1>()?),
        resize_buffers: shadow.original::<ResizeBuffers>()?,
        resize_buffers1: Some(shadow.original::<ResizeBuffers1>()?),
        set_color_space1: Some(shadow.original::<SetColorSpace1>()?),
    };
    shadow.replace::<QueryInterface>(swap_chain_query_interface_detour)?;
    shadow.replace::<Present>(present_detour)?;
    shadow.replace::<Present1>(present1_detour)?;
    shadow.replace::<ResizeBuffers>(resize_buffers_detour)?;
    shadow.replace::<SetColorSpace1>(set_color_space1_detour)?;
    shadow.replace::<ResizeBuffers1>(resize_buffers1_detour)?;
    let interface = if L::SLOT_COUNT == <DxgiSwapChain3 as ComInterfaceLayout>::SLOT_COUNT {
        SwapChainInterface::V3
    } else {
        SwapChainInterface::V4
    };
    // SAFETY: route registration precedes publication and owns the guard afterward.
    unsafe { publish_swap_chain(manager, pointer, interface, shadow, originals, wrap) }
}

unsafe fn publish_swap_chain<L>(
    manager: &Arc<Inner>,
    pointer: *mut c_void,
    _interface: SwapChainInterface,
    shadow: VtableShadow<L>,
    originals: SwapChainOriginals,
    wrap: fn(ThreadSafeInstalled<L>) -> HookGuard,
) -> Result<(), DxgiError>
where
    L: ComInterfaceLayout,
{
    let route = Route::SwapChain(SwapChainRoute {
        manager_id: manager.id,
        manager: Arc::downgrade(manager),
        originals,
    });
    register_route(pointer, &route)?;
    // SAFETY: the caller upholds VtableShadow's lifetime and matching-layout contract.
    let installed = match unsafe { shadow.install() } {
        Ok(installed) => installed,
        Err(error) => {
            unregister_route(manager.id, pointer);
            return Err(error.into());
        }
    };
    lock(&manager.hooks).push(wrap(ThreadSafeInstalled(installed)));
    Ok(())
}

fn register_route(pointer: *mut c_void, route: &Route) -> Result<(), DxgiError> {
    let mut routes = lock(routes());
    match routes.get(&(pointer as usize)) {
        Some(existing) if existing.manager_id() == route.manager_id() => return Ok(()),
        Some(_) => return Err(DxgiError::HookConflict),
        None => {}
    }
    routes.insert(pointer as usize, route.clone());
    Ok(())
}

fn unregister_route(manager_id: u64, pointer: *mut c_void) {
    let mut routes = lock(routes());
    if routes
        .get(&(pointer as usize))
        .is_some_and(|route| route.manager_id() == manager_id)
    {
        routes.remove(&(pointer as usize));
    }
}

unsafe extern "system" fn factory_query_interface_detour(
    this: *mut c_void,
    iid: *const c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: this function exactly matches the named SDK QueryInterface slot.
    unsafe { query_interface_boundary(this, iid, output, ObjectKind::Factory) }
}

unsafe extern "system" fn swap_chain_query_interface_detour(
    this: *mut c_void,
    iid: *const c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: this function exactly matches the named SDK QueryInterface slot.
    unsafe { query_interface_boundary(this, iid, output, ObjectKind::SwapChain) }
}

unsafe fn query_interface_boundary(
    this: *mut c_void,
    iid: *const c_void,
    output: *mut *mut c_void,
    kind: ObjectKind,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let boundary = match kind {
        ObjectKind::Factory => Boundary::FactoryQueryInterface,
        ObjectKind::SwapChain => Boundary::SwapChainQueryInterface,
    };
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let route = route_for(this, kind);
        let Some(route) = route else {
            return;
        };
        let (query, weak) = match route {
            Route::Factory(route) => (route.originals.query_interface, route.manager),
            Route::FactoryMedia(route) => (route.originals.query_interface, route.manager),
            Route::SwapChain(route) => (route.originals.query_interface, route.manager),
        };
        original = Some(query);
        manager = weak.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: the detour received the original ABI arguments unchanged.
        let native_result = unsafe { query(this, iid, output) };
        result = Some(native_result);
        if guard.is_none() || native_result < 0 || iid.is_null() || output.is_null() {
            return;
        }
        // SAFETY: a successful QueryInterface writes one live pointer.
        let returned = unsafe { output.read() };
        if returned.is_null() {
            return;
        }
        // SAFETY: non-null SDK IID pointer remains live for this call.
        let iid = unsafe { &*iid.cast::<GUID>() };
        let supported = match kind {
            ObjectKind::Factory => sdk::factory_interface(iid).is_some(),
            ObjectKind::SwapChain => sdk::swap_chain_interface(iid).is_some(),
        };
        if !supported {
            return;
        }
        if let Some(manager) = &manager {
            let attached = match kind {
                ObjectKind::Factory => {
                    // SAFETY: the successful native result proves returned implements iid.
                    unsafe { attach_factory(manager, returned, iid) }
                }
                ObjectKind::SwapChain => {
                    // SAFETY: the successful native result proves returned implements iid.
                    unsafe { attach_swap_chain(manager, returned, iid) }
                }
            };
            if let Err(error) = attached {
                manager.report_attach_error(kind, None, &error);
            }
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(boundary);
    }
    if let Some(result) = result {
        return result;
    }
    if let Some(original) = original {
        // SAFETY: the original was resolved from this concrete route and was not called yet.
        return unsafe { original(this, iid, output) };
    }
    // SAFETY: late cached detours read the restored IUnknown vtable by name.
    unsafe { fallback_query_interface(this, iid, output) }
}

unsafe extern "system" fn factory_create_swap_chain_detour(
    this: *mut c_void,
    device: *mut c_void,
    description: *const c_void,
    swap_chain: *mut *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = factory_route(this) else {
            return;
        };
        original = Some(route.originals.create_swap_chain);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result =
            unsafe { (route.originals.create_swap_chain)(this, device, description, swap_chain) };
        result = Some(native_result);
        if guard.is_some() {
            attach_created_swap_chain(
                manager.as_ref(),
                native_result,
                swap_chain,
                SwapChainInterface::Base,
            );
        }
    }));
    finish_factory_call(
        caught,
        result,
        manager.as_ref(),
        Boundary::FactoryCreateSwapChain,
        // SAFETY: the detour has not completed an original call; the typed route or restored vtable proves the signature.
        || unsafe {
            original
                .or_else(|| current_method::<DxgiFactory, CreateSwapChain>(this))
                .map_or(E_FAIL.0, |call| call(this, device, description, swap_chain))
        },
    )
}

unsafe extern "system" fn factory_create_swap_chain_for_hwnd_detour(
    this: *mut c_void,
    device: *mut c_void,
    window: *mut c_void,
    description: *const c_void,
    fullscreen_description: *const c_void,
    restrict_to_output: *mut c_void,
    swap_chain: *mut *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = factory_route(this) else {
            return;
        };
        let Some(call) = route.originals.create_for_hwnd else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe {
            call(
                this,
                device,
                window,
                description,
                fullscreen_description,
                restrict_to_output,
                swap_chain,
            )
        };
        result = Some(native_result);
        if guard.is_some() {
            attach_created_swap_chain(
                manager.as_ref(),
                native_result,
                swap_chain,
                SwapChainInterface::V1,
            );
        }
    }));
    finish_factory_call(
        caught,
        result,
        manager.as_ref(),
        Boundary::FactoryCreateSwapChain,
        // SAFETY: the detour has not completed an original call; the typed route or restored vtable proves the signature.
        || unsafe {
            original
                .or_else(|| current_method::<DxgiFactory2, CreateSwapChainForHwnd>(this))
                .map_or(E_FAIL.0, |call| {
                    call(
                        this,
                        device,
                        window,
                        description,
                        fullscreen_description,
                        restrict_to_output,
                        swap_chain,
                    )
                })
        },
    )
}

unsafe extern "system" fn factory_create_swap_chain_for_core_window_detour(
    this: *mut c_void,
    device: *mut c_void,
    window: *mut c_void,
    description: *const c_void,
    restrict_to_output: *mut c_void,
    swap_chain: *mut *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = factory_route(this) else {
            return;
        };
        let Some(call) = route.originals.create_for_core_window else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe {
            call(
                this,
                device,
                window,
                description,
                restrict_to_output,
                swap_chain,
            )
        };
        result = Some(native_result);
        if guard.is_some() {
            attach_created_swap_chain(
                manager.as_ref(),
                native_result,
                swap_chain,
                SwapChainInterface::V1,
            );
        }
    }));
    finish_factory_call(
        caught,
        result,
        manager.as_ref(),
        Boundary::FactoryCreateSwapChain,
        // SAFETY: the detour has not completed an original call; the typed route or restored vtable proves the signature.
        || unsafe {
            original
                .or_else(|| current_method::<DxgiFactory2, CreateSwapChainForCoreWindow>(this))
                .map_or(E_FAIL.0, |call| {
                    call(
                        this,
                        device,
                        window,
                        description,
                        restrict_to_output,
                        swap_chain,
                    )
                })
        },
    )
}

unsafe extern "system" fn factory_create_swap_chain_for_composition_detour(
    this: *mut c_void,
    device: *mut c_void,
    description: *const c_void,
    restrict_to_output: *mut c_void,
    swap_chain: *mut *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = factory_route(this) else {
            return;
        };
        let Some(call) = route.originals.create_for_composition else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result =
            unsafe { call(this, device, description, restrict_to_output, swap_chain) };
        result = Some(native_result);
        if guard.is_some() {
            attach_created_swap_chain(
                manager.as_ref(),
                native_result,
                swap_chain,
                SwapChainInterface::V1,
            );
        }
    }));
    finish_factory_call(
        caught,
        result,
        manager.as_ref(),
        Boundary::FactoryCreateSwapChain,
        // SAFETY: the detour has not completed an original call; the typed route or restored vtable proves the signature.
        || unsafe {
            original
                .or_else(|| current_method::<DxgiFactory2, CreateSwapChainForComposition>(this))
                .map_or(E_FAIL.0, |call| {
                    call(this, device, description, restrict_to_output, swap_chain)
                })
        },
    )
}

unsafe extern "system" fn factory_create_swap_chain_for_composition_surface_handle_detour(
    this: *mut c_void,
    device: *mut c_void,
    surface: *mut c_void,
    description: *const c_void,
    restrict_to_output: *mut c_void,
    swap_chain: *mut *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = factory_media_route(this) else {
            return;
        };
        let call = route.originals.create_for_composition_surface_handle;
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe {
            call(
                this,
                device,
                surface,
                description,
                restrict_to_output,
                swap_chain,
            )
        };
        result = Some(native_result);
        if guard.is_some() {
            attach_created_swap_chain(
                manager.as_ref(),
                native_result,
                swap_chain,
                SwapChainInterface::V1,
            );
        }
    }));
    finish_factory_call(
        caught,
        result,
        manager.as_ref(),
        Boundary::FactoryCreateSwapChain,
        // SAFETY: the detour has not completed an original call; the typed route or restored vtable proves the signature.
        || unsafe {
            original
                .or_else(|| {
                    current_method::<DxgiFactoryMedia, CreateSwapChainForCompositionSurfaceHandle>(
                        this,
                    )
                })
                .map_or(E_FAIL.0, |call| {
                    if call as usize
                        == factory_create_swap_chain_for_composition_surface_handle_detour
                            as *const () as usize
                    {
                        E_FAIL.0
                    } else {
                        call(
                            this,
                            device,
                            surface,
                            description,
                            restrict_to_output,
                            swap_chain,
                        )
                    }
                })
        },
    )
}

fn finish_factory_call(
    caught: Result<(), Box<dyn std::any::Any + Send>>,
    result: Option<i32>,
    manager: Option<&Arc<Inner>>,
    boundary: Boundary,
    fallback: impl FnOnce() -> i32,
) -> i32 {
    if caught.is_err()
        && let Some(manager) = manager
    {
        manager.report_panic(boundary);
    }
    result.unwrap_or_else(fallback)
}

fn attach_created_swap_chain(
    manager: Option<&Arc<Inner>>,
    result: i32,
    output: *mut *mut c_void,
    interface: SwapChainInterface,
) {
    if result < 0 || output.is_null() {
        return;
    }
    // SAFETY: successful DXGI creation writes one interface pointer.
    let pointer = unsafe { output.read() };
    if pointer.is_null() {
        return;
    }
    let Some(manager) = manager else {
        return;
    };
    let attached = {
        // SAFETY: the factory contract defines the returned interface version.
        unsafe { attach_swap_chain(manager, pointer, &sdk::swap_chain_iid(interface)) }
    };
    if let Err(error) = attached {
        manager.report_attach_error(ObjectKind::SwapChain, None, &error);
    }
}

unsafe extern "system" fn present_detour(this: *mut c_void, sync_interval: u32, flags: u32) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = swap_chain_route(this) else {
            return;
        };
        let call = route.originals.present;
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        let invocation = guard.as_ref().and_then(|_| {
            manager
                .as_ref()?
                .before_present(this, PresentMethod::Present)
        });
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe { call(this, sync_interval, flags) };
        result = Some(native_result);
        if guard.is_some()
            && let Some(manager) = &manager
        {
            manager.after_present(this, PresentMethod::Present, invocation, native_result);
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(Boundary::Present);
    }
    if let Some(result) = result {
        return result;
    }
    if let Some(call) = original {
        // SAFETY: this original was resolved before the panic and was not called yet.
        return unsafe { call(this, sync_interval, flags) };
    }
    // SAFETY: route absence is possible only after restoration or on invalid input.
    unsafe {
        current_method::<DxgiSwapChain, Present>(this).map_or(E_FAIL.0, |call| {
            if call as usize == present_detour as *const () as usize {
                E_FAIL.0
            } else {
                call(this, sync_interval, flags)
            }
        })
    }
}

unsafe extern "system" fn present1_detour(
    this: *mut c_void,
    sync_interval: u32,
    flags: u32,
    parameters: *const c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = swap_chain_route(this) else {
            return;
        };
        let Some(call) = route.originals.present1 else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        let invocation = guard.as_ref().and_then(|_| {
            manager
                .as_ref()?
                .before_present(this, PresentMethod::Present1)
        });
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe { call(this, sync_interval, flags, parameters) };
        result = Some(native_result);
        if guard.is_some()
            && let Some(manager) = &manager
        {
            manager.after_present(this, PresentMethod::Present1, invocation, native_result);
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(Boundary::Present);
    }
    if let Some(result) = result {
        result
    } else {
        // SAFETY: no original call completed; resolve the restored named method if needed.
        unsafe {
            original
                .or_else(|| current_method::<DxgiSwapChain1, Present1>(this))
                .map_or(E_FAIL.0, |call| {
                    call(this, sync_interval, flags, parameters)
                })
        }
    }
}

unsafe extern "system" fn resize_buffers_detour(
    this: *mut c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    format: i32,
    flags: u32,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = swap_chain_route(this) else {
            return;
        };
        let call = route.originals.resize_buffers;
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        let invocation = guard
            .as_ref()
            .and_then(|_| manager.as_ref()?.before_resize(this, width, height, format));
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe { call(this, buffer_count, width, height, format, flags) };
        result = Some(native_result);
        if guard.is_some()
            && let Some(manager) = &manager
        {
            manager.after_resize(this, invocation, native_result);
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(Boundary::ResizeBuffers);
    }
    if let Some(result) = result {
        return result;
    }
    if let Some(call) = original {
        // SAFETY: this original was resolved before the panic and was not called yet.
        return unsafe { call(this, buffer_count, width, height, format, flags) };
    }
    // SAFETY: route absence is possible only after restoration or on invalid input.
    unsafe {
        current_method::<DxgiSwapChain, ResizeBuffers>(this).map_or(E_FAIL.0, |call| {
            if call as usize == resize_buffers_detour as *const () as usize {
                E_FAIL.0
            } else {
                call(this, buffer_count, width, height, format, flags)
            }
        })
    }
}

unsafe extern "system" fn set_color_space1_detour(this: *mut c_void, color_space: i32) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = swap_chain_route(this) else {
            return;
        };
        let Some(call) = route.originals.set_color_space1 else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe { call(this, color_space) };
        result = Some(native_result);
        if guard.is_some()
            && let Some(manager) = &manager
        {
            manager.after_set_color_space(this, color_space, native_result);
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(Boundary::SetColorSpace1);
    }
    if let Some(result) = result {
        return result;
    }
    if let Some(call) = original {
        // SAFETY: this original was resolved before the panic and was not called yet.
        return unsafe { call(this, color_space) };
    }
    // SAFETY: route absence is possible only after restoration or on invalid input.
    unsafe {
        current_method::<DxgiSwapChain3, SetColorSpace1>(this).map_or(E_FAIL.0, |call| {
            if call as usize == set_color_space1_detour as *const () as usize {
                E_FAIL.0
            } else {
                call(this, color_space)
            }
        })
    }
}

unsafe extern "system" fn resize_buffers1_detour(
    this: *mut c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    format: i32,
    flags: u32,
    creation_node_mask: *const u32,
    present_queue: *const *mut c_void,
) -> i32 {
    let mut result = None;
    let mut original = None;
    let mut manager = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let Some(route) = swap_chain_route(this) else {
            return;
        };
        let Some(call) = route.originals.resize_buffers1 else {
            return;
        };
        original = Some(call);
        manager = route.manager.upgrade();
        let guard = manager
            .as_ref()
            .and_then(|manager| manager.gate.try_enter());
        let invocation = guard
            .as_ref()
            .and_then(|_| manager.as_ref()?.before_resize(this, width, height, format));
        // SAFETY: arguments are forwarded unchanged to the exact original slot.
        let native_result = unsafe {
            call(
                this,
                buffer_count,
                width,
                height,
                format,
                flags,
                creation_node_mask,
                present_queue,
            )
        };
        result = Some(native_result);
        if guard.is_some()
            && let Some(manager) = &manager
        {
            manager.after_resize(this, invocation, native_result);
        }
    }));
    if caught.is_err()
        && let Some(manager) = &manager
    {
        manager.report_panic(Boundary::ResizeBuffers);
    }
    if let Some(result) = result {
        return result;
    }
    if let Some(call) = original {
        // SAFETY: this original was resolved before the panic and was not called yet.
        return unsafe {
            call(
                this,
                buffer_count,
                width,
                height,
                format,
                flags,
                creation_node_mask,
                present_queue,
            )
        };
    }
    // SAFETY: route absence is possible only after restoration or on invalid input.
    unsafe {
        current_method::<DxgiSwapChain3, ResizeBuffers1>(this).map_or(E_FAIL.0, |call| {
            if call as usize == resize_buffers1_detour as *const () as usize {
                E_FAIL.0
            } else {
                call(
                    this,
                    buffer_count,
                    width,
                    height,
                    format,
                    flags,
                    creation_node_mask,
                    present_queue,
                )
            }
        })
    }
}

fn factory_route(pointer: *mut c_void) -> Option<FactoryRoute> {
    match lock(routes()).get(&(pointer as usize)).cloned()? {
        Route::Factory(route) => Some(route),
        Route::FactoryMedia(_) | Route::SwapChain(_) => None,
    }
}

fn factory_media_route(pointer: *mut c_void) -> Option<FactoryMediaRoute> {
    match lock(routes()).get(&(pointer as usize)).cloned()? {
        Route::FactoryMedia(route) => Some(route),
        Route::Factory(_) | Route::SwapChain(_) => None,
    }
}

fn swap_chain_route(pointer: *mut c_void) -> Option<SwapChainRoute> {
    match lock(routes()).get(&(pointer as usize)).cloned()? {
        Route::SwapChain(route) => Some(route),
        Route::Factory(_) | Route::FactoryMedia(_) => None,
    }
}

fn route_for(pointer: *mut c_void, kind: ObjectKind) -> Option<Route> {
    let route = lock(routes()).get(&(pointer as usize)).cloned()?;
    match (&route, kind) {
        (Route::Factory(_) | Route::FactoryMedia(_), ObjectKind::Factory)
        | (Route::SwapChain(_), ObjectKind::SwapChain) => Some(route),
        (Route::Factory(_) | Route::FactoryMedia(_), ObjectKind::SwapChain)
        | (Route::SwapChain(_), ObjectKind::Factory) => None,
    }
}

unsafe fn current_method<L, M>(pointer: *mut c_void) -> Option<M::Function>
where
    L: ComInterfaceLayout,
    M: ComMethod<L>,
{
    // SAFETY: this fallback is used only after a typed hook was restored on the
    // same concrete pointer. The named layout remains the dispatch contract.
    let shadow = unsafe { VtableShadow::<L>::copy_from(pointer) }.ok()?;
    shadow.original::<M>().ok()
}

unsafe fn fallback_query_interface(
    this: *mut c_void,
    iid: *const c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: a late cached detour still receives the original live COM object;
    // after restore, its named IUnknown vtable contains the native method.
    let Some((query, _)) = (unsafe { sdk::original_iunknown_methods(this) }) else {
        return E_FAIL.0;
    };
    if query as usize == factory_query_interface_detour as *const () as usize
        || query as usize == swap_chain_query_interface_detour as *const () as usize
    {
        return E_FAIL.0;
    }
    // SAFETY: arguments are unchanged and this function has not called native QI yet.
    unsafe { query(this, iid, output) }
}

fn routes() -> &'static Mutex<HashMap<usize, Route>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        ptr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicI32, AtomicPtr, AtomicU32, Ordering},
        },
        time::Duration,
    };

    use nexus_hook::{
        ComInterfaceLayout, VtableShadow,
        dxgi::{DxgiFactoryMedia, DxgiSwapChain, DxgiSwapChain1, DxgiSwapChain3},
    };
    use nexus_render::ColorSpace;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;

    use super::*;
    use crate::{DxgiCallbacks, DxgiConfig, DxgiInterceptionManager};

    static PRESENT_CALLS: AtomicU32 = AtomicU32::new(0);
    static QUERY_CALLS: AtomicU32 = AtomicU32::new(0);
    static REFERENCES: AtomicU32 = AtomicU32::new(1);

    static FACTORY_CREATE_CALLS: AtomicU32 = AtomicU32::new(0);
    static FACTORY_MEDIA_CREATE_CALLS: AtomicU32 = AtomicU32::new(0);
    static FACTORY_MEDIA_POINTER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

    static FAKE_COM_TEST_LOCK: Mutex<()> = Mutex::new(());

    static RESIZE_CALLS: AtomicU32 = AtomicU32::new(0);
    static SET_COLOR_SPACE_CALLS: AtomicU32 = AtomicU32::new(0);
    static SET_COLOR_SPACE_RESULT: AtomicI32 = AtomicI32::new(0);
    static SWAP_CHAIN_MAX_INTERFACE: AtomicU32 = AtomicU32::new(0);

    #[repr(C)]
    struct FakeSwapChain {
        vtable: *const *const c_void,
    }

    #[repr(C)]
    struct FakeFactory {
        vtable: *const *const c_void,
        swap_chain: *mut c_void,
    }

    unsafe extern "system" fn fake_query_interface(
        this: *mut c_void,
        iid: *const c_void,
        output: *mut *mut c_void,
    ) -> i32 {
        QUERY_CALLS.fetch_add(1, Ordering::Relaxed);
        if iid.is_null() || output.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: the test invokes this function with a sys GUID.
        let iid = unsafe { &*iid.cast::<GUID>() };
        let supported = sdk::swap_chain_interface(iid).is_some_and(|interface| {
            u32::from(interface as u8) <= SWAP_CHAIN_MAX_INTERFACE.load(Ordering::Relaxed)
        });
        if supported {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(this) };
            REFERENCES.fetch_add(1, Ordering::Relaxed);
            0
        } else {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(ptr::null_mut()) };
            windows::Win32::Foundation::E_NOINTERFACE.0
        }
    }

    unsafe extern "system" fn fake_factory_query_interface(
        this: *mut c_void,
        iid: *const c_void,
        output: *mut *mut c_void,
    ) -> i32 {
        QUERY_CALLS.fetch_add(1, Ordering::Relaxed);
        if iid.is_null() || output.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: the test invokes this function with a sys GUID.
        let iid = unsafe { &*iid.cast::<GUID>() };
        let returned = match sdk::factory_interface(iid) {
            Some(FactoryInterface::Base) => this,
            Some(FactoryInterface::Media) => FACTORY_MEDIA_POINTER.load(Ordering::Relaxed),
            Some(
                FactoryInterface::V1
                | FactoryInterface::V2
                | FactoryInterface::V3
                | FactoryInterface::V4
                | FactoryInterface::V5
                | FactoryInterface::V6
                | FactoryInterface::V7,
            )
            | None => ptr::null_mut(),
        };
        if !returned.is_null() {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(returned) };
            REFERENCES.fetch_add(1, Ordering::Relaxed);
            0
        } else {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(ptr::null_mut()) };
            windows::Win32::Foundation::E_NOINTERFACE.0
        }
    }

    unsafe extern "system" fn fake_factory_media_query_interface(
        this: *mut c_void,
        iid: *const c_void,
        output: *mut *mut c_void,
    ) -> i32 {
        QUERY_CALLS.fetch_add(1, Ordering::Relaxed);
        if iid.is_null() || output.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: the test invokes this function with a sys GUID.
        let iid = unsafe { &*iid.cast::<GUID>() };
        if sdk::factory_interface(iid) == Some(FactoryInterface::Media) {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(this) };
            REFERENCES.fetch_add(1, Ordering::Relaxed);
            0
        } else {
            // SAFETY: output is validated above and points to writable storage.
            unsafe { output.write(ptr::null_mut()) };
            windows::Win32::Foundation::E_NOINTERFACE.0
        }
    }

    unsafe extern "system" fn fake_create_swap_chain(
        this: *mut c_void,
        _device: *mut c_void,
        _description: *const c_void,
        output: *mut *mut c_void,
    ) -> i32 {
        FACTORY_CREATE_CALLS.fetch_add(1, Ordering::Relaxed);
        if this.is_null() || output.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: the test passes its live repr(C) fake factory as `this`.
        let factory = unsafe { &*this.cast::<FakeFactory>() };
        if factory.swap_chain.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: output is validated above and the fake swap chain remains live.
        unsafe { output.write(factory.swap_chain) };
        REFERENCES.fetch_add(1, Ordering::Relaxed);
        0
    }

    unsafe extern "system" fn fake_create_swap_chain_for_composition_surface_handle(
        this: *mut c_void,
        _device: *mut c_void,
        _surface: *mut c_void,
        _description: *const c_void,
        _restrict_to_output: *mut c_void,
        output: *mut *mut c_void,
    ) -> i32 {
        FACTORY_MEDIA_CREATE_CALLS.fetch_add(1, Ordering::Relaxed);
        if this.is_null() || output.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: the test passes its live repr(C) fake factory as `this`.
        let factory = unsafe { &*this.cast::<FakeFactory>() };
        if factory.swap_chain.is_null() {
            return E_FAIL.0;
        }
        // SAFETY: output is validated above and the fake swap chain remains live.
        unsafe { output.write(factory.swap_chain) };
        REFERENCES.fetch_add(1, Ordering::Relaxed);
        0
    }

    unsafe extern "system" fn fake_add_ref(_this: *mut c_void) -> u32 {
        REFERENCES.fetch_add(1, Ordering::Relaxed).saturating_add(1)
    }

    unsafe extern "system" fn fake_release(_this: *mut c_void) -> u32 {
        REFERENCES.fetch_sub(1, Ordering::Relaxed).saturating_sub(1)
    }

    unsafe extern "system" fn fake_unused() -> i32 {
        E_FAIL.0
    }

    unsafe extern "system" fn fake_present(
        _this: *mut c_void,
        _sync_interval: u32,
        _flags: u32,
    ) -> i32 {
        PRESENT_CALLS.fetch_add(1, Ordering::Relaxed);
        0
    }

    unsafe extern "system" fn fake_present1(
        _this: *mut c_void,
        _sync_interval: u32,
        _flags: u32,
        _present_parameters: *const c_void,
    ) -> i32 {
        0
    }

    unsafe extern "system" fn fake_resize(
        _this: *mut c_void,
        _buffer_count: u32,
        _width: u32,
        _height: u32,
        _format: i32,
        _flags: u32,
    ) -> i32 {
        RESIZE_CALLS.fetch_add(1, Ordering::Relaxed);
        0
    }

    unsafe extern "system" fn fake_resize1(
        _this: *mut c_void,
        _buffer_count: u32,
        _width: u32,
        _height: u32,
        _format: i32,
        _flags: u32,
        _creation_node_mask: *const u32,
        _present_queue: *const *mut c_void,
    ) -> i32 {
        0
    }

    unsafe extern "system" fn fake_set_color_space1(_this: *mut c_void, _color_space: i32) -> i32 {
        SET_COLOR_SPACE_CALLS.fetch_add(1, Ordering::Relaxed);
        SET_COLOR_SPACE_RESULT.load(Ordering::Relaxed)
    }

    struct PanickingObserver {
        events: Mutex<u32>,
    }

    impl DxgiCallbacks for PanickingObserver {
        fn observation(&self, _event: DxgiObservationEvent) {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *events = events.saturating_add(1);
            panic!("observer panic must not cross COM");
        }
    }

    struct RecordingObserver {
        events: Mutex<Vec<DxgiObservationEvent>>,
    }

    impl RecordingObserver {
        fn events(&self) -> Vec<DxgiObservationEvent> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl DxgiCallbacks for RecordingObserver {
        fn observation(&self, event: DxgiObservationEvent) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    fn fake_vtable() -> Box<[*const c_void]> {
        let mut entries = vec![fake_unused as *const c_void; DxgiSwapChain::SLOT_COUNT];
        entries[<nexus_hook::QueryInterface as ComMethod<DxgiSwapChain>>::INDEX] =
            fake_query_interface as *const c_void;
        entries[<nexus_hook::AddRef as ComMethod<DxgiSwapChain>>::INDEX] =
            fake_add_ref as *const c_void;
        entries[<nexus_hook::Release as ComMethod<DxgiSwapChain>>::INDEX] =
            fake_release as *const c_void;
        entries[<Present as ComMethod<DxgiSwapChain>>::INDEX] = fake_present as *const c_void;
        entries[<ResizeBuffers as ComMethod<DxgiSwapChain>>::INDEX] = fake_resize as *const c_void;
        entries.into_boxed_slice()
    }

    fn fake_swap_chain1_vtable() -> Box<[*const c_void]> {
        let mut entries = vec![fake_unused as *const c_void; DxgiSwapChain1::SLOT_COUNT];
        entries[<nexus_hook::QueryInterface as ComMethod<DxgiSwapChain1>>::INDEX] =
            fake_query_interface as *const c_void;
        entries[<nexus_hook::AddRef as ComMethod<DxgiSwapChain1>>::INDEX] =
            fake_add_ref as *const c_void;
        entries[<nexus_hook::Release as ComMethod<DxgiSwapChain1>>::INDEX] =
            fake_release as *const c_void;
        entries[<Present as ComMethod<DxgiSwapChain1>>::INDEX] = fake_present as *const c_void;
        entries[<Present1 as ComMethod<DxgiSwapChain1>>::INDEX] = fake_present1 as *const c_void;
        entries[<ResizeBuffers as ComMethod<DxgiSwapChain1>>::INDEX] = fake_resize as *const c_void;
        entries.into_boxed_slice()
    }

    fn fake_swap_chain3_vtable() -> Box<[*const c_void]> {
        let mut entries = vec![fake_unused as *const c_void; DxgiSwapChain3::SLOT_COUNT];
        entries[<nexus_hook::QueryInterface as ComMethod<DxgiSwapChain3>>::INDEX] =
            fake_query_interface as *const c_void;
        entries[<nexus_hook::AddRef as ComMethod<DxgiSwapChain3>>::INDEX] =
            fake_add_ref as *const c_void;
        entries[<nexus_hook::Release as ComMethod<DxgiSwapChain3>>::INDEX] =
            fake_release as *const c_void;
        entries[<Present as ComMethod<DxgiSwapChain3>>::INDEX] = fake_present as *const c_void;
        entries[<Present1 as ComMethod<DxgiSwapChain3>>::INDEX] = fake_present1 as *const c_void;
        entries[<ResizeBuffers as ComMethod<DxgiSwapChain3>>::INDEX] = fake_resize as *const c_void;
        entries[<SetColorSpace1 as ComMethod<DxgiSwapChain3>>::INDEX] =
            fake_set_color_space1 as *const c_void;
        entries[<ResizeBuffers1 as ComMethod<DxgiSwapChain3>>::INDEX] =
            fake_resize1 as *const c_void;
        entries.into_boxed_slice()
    }

    fn fake_factory_vtable() -> Box<[*const c_void]> {
        let mut entries = vec![fake_unused as *const c_void; DxgiFactory::SLOT_COUNT];
        entries[<nexus_hook::QueryInterface as ComMethod<DxgiFactory>>::INDEX] =
            fake_factory_query_interface as *const c_void;
        entries[<nexus_hook::AddRef as ComMethod<DxgiFactory>>::INDEX] =
            fake_add_ref as *const c_void;
        entries[<nexus_hook::Release as ComMethod<DxgiFactory>>::INDEX] =
            fake_release as *const c_void;
        entries[<CreateSwapChain as ComMethod<DxgiFactory>>::INDEX] =
            fake_create_swap_chain as *const c_void;
        entries.into_boxed_slice()
    }

    fn fake_factory_media_vtable() -> Box<[*const c_void]> {
        let mut entries = vec![fake_unused as *const c_void; DxgiFactoryMedia::SLOT_COUNT];
        entries[<nexus_hook::QueryInterface as ComMethod<DxgiFactoryMedia>>::INDEX] =
            fake_factory_media_query_interface as *const c_void;
        entries[<nexus_hook::AddRef as ComMethod<DxgiFactoryMedia>>::INDEX] =
            fake_add_ref as *const c_void;
        entries[<nexus_hook::Release as ComMethod<DxgiFactoryMedia>>::INDEX] =
            fake_release as *const c_void;
        entries
            [<CreateSwapChainForCompositionSurfaceHandle as ComMethod<DxgiFactoryMedia>>::INDEX] =
            fake_create_swap_chain_for_composition_surface_handle as *const c_void;
        entries.into_boxed_slice()
    }

    struct PanickingRenderer;

    impl crate::OverlayRenderer for PanickingRenderer {
        fn render(
            &self,
            _frame: &crate::PresentFrame<'_>,
        ) -> Result<(), crate::RenderCallbackError> {
            Ok(())
        }

        fn before_resize(
            &self,
            _frame: &crate::ResizeFrame<'_>,
        ) -> Result<(), crate::RenderCallbackError> {
            panic!("renderer panic must not cross COM");
        }
    }

    #[test]
    fn observer_panic_does_not_double_call_present_and_shutdown_restores() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        PRESENT_CALLS.store(0, Ordering::Relaxed);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(1, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE
            .store(u32::from(SwapChainInterface::Base as u8), Ordering::Relaxed);
        let vtable = fake_vtable();
        let original_vtable = vtable.as_ptr();
        let mut object = FakeSwapChain {
            vtable: original_vtable,
        };
        let observer = Arc::new(PanickingObserver {
            events: Mutex::new(0),
        });
        let manager = DxgiInterceptionManager::new(DxgiConfig::default(), observer, None);

        // SAFETY: the fake object publishes a complete base swap-chain layout.
        let outcome = unsafe {
            manager.attach_swap_chain(
                (&mut object as *mut FakeSwapChain).cast(),
                &sdk::swap_chain_iid(SwapChainInterface::Base),
            )
        }
        .expect("fake swap chain should attach");
        assert!(matches!(outcome, AttachOutcome::Attached { .. }));

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe {
            VtableShadow::<DxgiSwapChain>::copy_from((&mut object as *mut FakeSwapChain).cast())
        }
        .expect("published shadow should be readable");
        let present = shadow
            .published::<Present>()
            .expect("present marker should decode");
        // SAFETY: the typed marker supplies the exact function signature.
        let result = unsafe { present((&mut object as *mut FakeSwapChain).cast(), 0, 0) };
        assert_eq!(result, 0);
        assert_eq!(PRESENT_CALLS.load(Ordering::Relaxed), 1);

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(object.vtable, original_vtable));
    }

    #[test]
    fn renderer_panic_does_not_double_call_resize() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        RESIZE_CALLS.store(0, Ordering::Relaxed);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(1, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE
            .store(u32::from(SwapChainInterface::Base as u8), Ordering::Relaxed);
        let vtable = fake_vtable();
        let original_vtable = vtable.as_ptr();
        let mut object = FakeSwapChain {
            vtable: original_vtable,
        };
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(PanickingObserver {
                events: Mutex::new(0),
            }),
            Some(Arc::new(PanickingRenderer)),
        );

        // SAFETY: the fake object publishes a complete base swap-chain layout.
        unsafe {
            manager.attach_swap_chain(
                (&mut object as *mut FakeSwapChain).cast(),
                &sdk::swap_chain_iid(SwapChainInterface::Base),
            )
        }
        .expect("fake swap chain should attach");

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe {
            VtableShadow::<DxgiSwapChain>::copy_from((&mut object as *mut FakeSwapChain).cast())
        }
        .expect("published shadow should be readable");
        let resize = shadow
            .published::<ResizeBuffers>()
            .expect("resize marker should decode");
        // SAFETY: the typed marker supplies the exact function signature.
        let result = unsafe {
            resize(
                (&mut object as *mut FakeSwapChain).cast(),
                2,
                1280,
                720,
                0,
                0,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(RESIZE_CALLS.load(Ordering::Relaxed), 1);

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(object.vtable, original_vtable));
    }

    #[test]
    fn hooked_query_interface_calls_native_implementation_once() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(1, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE
            .store(u32::from(SwapChainInterface::Base as u8), Ordering::Relaxed);
        let vtable = fake_vtable();
        let mut object = FakeSwapChain {
            vtable: vtable.as_ptr(),
        };
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(PanickingObserver {
                events: Mutex::new(0),
            }),
            None,
        );
        // SAFETY: the fake object publishes a complete base swap-chain layout.
        unsafe {
            manager.attach_swap_chain(
                (&mut object as *mut FakeSwapChain).cast(),
                &sdk::swap_chain_iid(SwapChainInterface::Base),
            )
        }
        .expect("fake swap chain should attach");
        QUERY_CALLS.store(0, Ordering::Relaxed);

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe {
            VtableShadow::<DxgiSwapChain>::copy_from((&mut object as *mut FakeSwapChain).cast())
        }
        .expect("published shadow should be readable");
        let query = shadow
            .published::<QueryInterface>()
            .expect("query marker should decode");
        let mut output = ptr::null_mut();
        let iid = sdk::swap_chain_iid(SwapChainInterface::Base);
        // SAFETY: the typed marker supplies the exact signature and writable output.
        let result = unsafe {
            query(
                (&mut object as *mut FakeSwapChain).cast(),
                (&iid as *const GUID).cast(),
                &mut output,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(QUERY_CALLS.load(Ordering::Relaxed), 1);
        // SAFETY: successful fake QueryInterface returned one owned reference.
        unsafe { fake_release(output) };
        let _ = manager.close_and_drain(Duration::from_secs(1));
    }

    #[test]
    fn factory_create_swap_chain_calls_native_once_and_auto_attaches_result() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        FACTORY_CREATE_CALLS.store(0, Ordering::Relaxed);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(2, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE
            .store(u32::from(SwapChainInterface::Base as u8), Ordering::Relaxed);

        let swap_chain_vtable = fake_vtable();
        let original_swap_chain_vtable = swap_chain_vtable.as_ptr();
        let mut swap_chain = FakeSwapChain {
            vtable: original_swap_chain_vtable,
        };
        let factory_vtable = fake_factory_vtable();
        let original_factory_vtable = factory_vtable.as_ptr();
        let mut factory = FakeFactory {
            vtable: original_factory_vtable,
            swap_chain: (&mut swap_chain as *mut FakeSwapChain).cast(),
        };
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(PanickingObserver {
                events: Mutex::new(0),
            }),
            None,
        );

        // SAFETY: the fake object publishes a complete base factory layout.
        unsafe {
            manager.attach_factory(
                (&mut factory as *mut FakeFactory).cast(),
                &sdk::factory_iid(FactoryInterface::Base),
            )
        }
        .expect("fake factory should attach");

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe {
            VtableShadow::<DxgiFactory>::copy_from((&mut factory as *mut FakeFactory).cast())
        }
        .expect("published factory shadow should be readable");
        let create = shadow
            .published::<CreateSwapChain>()
            .expect("factory create marker should decode");
        let mut output = ptr::null_mut();
        // SAFETY: the typed marker supplies the exact signature and writable output.
        let result = unsafe {
            create(
                (&mut factory as *mut FakeFactory).cast(),
                ptr::null_mut(),
                ptr::null(),
                &mut output,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(FACTORY_CREATE_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(output, (&mut swap_chain as *mut FakeSwapChain).cast());
        assert!(!ptr::eq(swap_chain.vtable, original_swap_chain_vtable));

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(factory.vtable, original_factory_vtable));
        assert!(ptr::eq(swap_chain.vtable, original_swap_chain_vtable));
        // SAFETY: successful fake creation returned one owned reference.
        unsafe { fake_release(output) };
    }

    #[test]
    fn set_color_space_updates_only_after_success_and_calls_native_once() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(1, Ordering::Relaxed);
        SET_COLOR_SPACE_CALLS.store(0, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE.store(u32::from(SwapChainInterface::V3 as u8), Ordering::Relaxed);

        let vtable = fake_swap_chain3_vtable();
        let original_vtable = vtable.as_ptr();
        let mut object = FakeSwapChain {
            vtable: original_vtable,
        };
        let pointer = (&mut object as *mut FakeSwapChain).cast();
        let observer = Arc::new(RecordingObserver {
            events: Mutex::new(Vec::new()),
        });
        let manager = DxgiInterceptionManager::new(DxgiConfig::default(), observer.clone(), None);

        // SAFETY: the fake object publishes a complete swap-chain-3 layout.
        unsafe { manager.attach_swap_chain(pointer, &sdk::swap_chain_iid(SwapChainInterface::V3)) }
            .expect("fake swap chain 3 should attach");
        let manager_id = swap_chain_route(pointer)
            .expect("attached swap chain route should exist")
            .manager_id;

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe { VtableShadow::<DxgiSwapChain3>::copy_from(pointer) }
            .expect("published swap-chain-3 shadow should be readable");
        let set_color_space = shadow
            .published::<SetColorSpace1>()
            .expect("color-space marker should decode");

        SET_COLOR_SPACE_RESULT.store(E_FAIL.0, Ordering::Relaxed);
        // SAFETY: the typed marker supplies the exact function signature.
        let initial_failure = unsafe { set_color_space(pointer, 77) };
        assert_eq!(initial_failure, E_FAIL.0);
        assert_eq!(SET_COLOR_SPACE_CALLS.load(Ordering::Relaxed), 1);

        SET_COLOR_SPACE_RESULT.store(0, Ordering::Relaxed);
        // SAFETY: the typed marker supplies the exact function signature.
        let success =
            unsafe { set_color_space(pointer, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0) };
        assert_eq!(success, 0);
        assert_eq!(SET_COLOR_SPACE_CALLS.load(Ordering::Relaxed), 2);

        SET_COLOR_SPACE_RESULT.store(E_FAIL.0, Ordering::Relaxed);
        // SAFETY: the typed marker supplies the exact function signature.
        let later_failure = unsafe { set_color_space(pointer, 12_345) };
        assert_eq!(later_failure, E_FAIL.0);
        assert_eq!(SET_COLOR_SPACE_CALLS.load(Ordering::Relaxed), 3);

        let forwarded: Vec<_> = observer
            .events()
            .into_iter()
            .filter_map(|event| match event {
                DxgiObservationEvent::ColorSpaceForwarded {
                    requested,
                    active,
                    result,
                    ..
                } => Some((requested, active, result)),
                _ => None,
            })
            .collect();
        assert_eq!(
            forwarded,
            vec![
                (
                    ColorSpace::Other(77),
                    ColorSpace::Other(sdk::UNKNOWN_COLOR_SPACE),
                    crate::HResultDisposition::Other(E_FAIL.0),
                ),
                (
                    ColorSpace::Hdr10Pq,
                    ColorSpace::Hdr10Pq,
                    crate::HResultDisposition::Success,
                ),
                (
                    ColorSpace::Other(12_345),
                    ColorSpace::Hdr10Pq,
                    crate::HResultDisposition::Other(E_FAIL.0),
                ),
            ]
        );

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(object.vtable, original_vtable));
        unregister_route(manager_id, pointer);
    }

    #[test]
    fn observer_panic_during_set_color_space_is_contained() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(1, Ordering::Relaxed);
        SET_COLOR_SPACE_CALLS.store(0, Ordering::Relaxed);
        SET_COLOR_SPACE_RESULT.store(0, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE.store(u32::from(SwapChainInterface::V3 as u8), Ordering::Relaxed);

        let vtable = fake_swap_chain3_vtable();
        let original_vtable = vtable.as_ptr();
        let mut object = FakeSwapChain {
            vtable: original_vtable,
        };
        let pointer = (&mut object as *mut FakeSwapChain).cast();
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(PanickingObserver {
                events: Mutex::new(0),
            }),
            None,
        );

        // SAFETY: the fake object publishes a complete swap-chain-3 layout.
        unsafe { manager.attach_swap_chain(pointer, &sdk::swap_chain_iid(SwapChainInterface::V3)) }
            .expect("fake swap chain 3 should attach");
        let manager_id = swap_chain_route(pointer)
            .expect("attached swap chain route should exist")
            .manager_id;
        // SAFETY: attachment proves the object now has a complete typed shadow.
        let shadow = unsafe { VtableShadow::<DxgiSwapChain3>::copy_from(pointer) }
            .expect("published swap-chain-3 shadow should be readable");
        let set_color_space = shadow
            .published::<SetColorSpace1>()
            .expect("color-space marker should decode");

        // SAFETY: the typed marker supplies the exact function signature.
        let result =
            unsafe { set_color_space(pointer, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0) };
        assert_eq!(result, 0);
        assert_eq!(SET_COLOR_SPACE_CALLS.load(Ordering::Relaxed), 1);

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(object.vtable, original_vtable));
        unregister_route(manager_id, pointer);
    }

    #[test]
    fn media_interface_from_query_is_hooked_and_attaches_created_swap_chain() {
        let _serial = lock(&FAKE_COM_TEST_LOCK);
        FACTORY_MEDIA_CREATE_CALLS.store(0, Ordering::Relaxed);
        QUERY_CALLS.store(0, Ordering::Relaxed);
        REFERENCES.store(3, Ordering::Relaxed);
        SWAP_CHAIN_MAX_INTERFACE.store(u32::from(SwapChainInterface::V1 as u8), Ordering::Relaxed);

        let swap_chain_vtable = fake_swap_chain1_vtable();
        let original_swap_chain_vtable = swap_chain_vtable.as_ptr();
        let mut swap_chain = FakeSwapChain {
            vtable: original_swap_chain_vtable,
        };
        let media_vtable = fake_factory_media_vtable();
        let original_media_vtable = media_vtable.as_ptr();
        let mut media = FakeFactory {
            vtable: original_media_vtable,
            swap_chain: (&mut swap_chain as *mut FakeSwapChain).cast(),
        };
        let factory_vtable = fake_factory_vtable();
        let original_factory_vtable = factory_vtable.as_ptr();
        let mut factory = FakeFactory {
            vtable: original_factory_vtable,
            swap_chain: ptr::null_mut(),
        };
        let factory_pointer = (&mut factory as *mut FakeFactory).cast();
        let media_pointer = (&mut media as *mut FakeFactory).cast();
        let swap_chain_pointer = (&mut swap_chain as *mut FakeSwapChain).cast();
        FACTORY_MEDIA_POINTER.store(media_pointer, Ordering::Relaxed);

        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(PanickingObserver {
                events: Mutex::new(0),
            }),
            None,
        );
        // SAFETY: the fake object publishes a complete base factory layout.
        unsafe {
            manager.attach_factory(factory_pointer, &sdk::factory_iid(FactoryInterface::Base))
        }
        .expect("fake base factory should attach");
        let manager_id = factory_route(factory_pointer)
            .expect("attached factory route should exist")
            .manager_id;

        // SAFETY: attachment proves the object now has a complete typed shadow.
        let factory_shadow = unsafe { VtableShadow::<DxgiFactory>::copy_from(factory_pointer) }
            .expect("published factory shadow should be readable");
        let query = factory_shadow
            .published::<QueryInterface>()
            .expect("factory query marker should decode");
        let media_iid = sdk::factory_iid(FactoryInterface::Media);
        let mut media_output = ptr::null_mut();
        // SAFETY: the typed marker supplies the exact signature and writable output.
        let query_result = unsafe {
            query(
                factory_pointer,
                (&media_iid as *const GUID).cast(),
                &mut media_output,
            )
        };
        assert_eq!(query_result, 0);
        assert_eq!(media_output, media_pointer);
        assert!(!ptr::eq(media.vtable, original_media_vtable));

        // SAFETY: successful QueryInterface classification proves FactoryMedia.
        let media_shadow = unsafe { VtableShadow::<DxgiFactoryMedia>::copy_from(media_output) }
            .expect("published media shadow should be readable");
        let create = media_shadow
            .published::<CreateSwapChainForCompositionSurfaceHandle>()
            .expect("composition-surface-handle marker should decode");
        let mut swap_chain_output = ptr::null_mut();
        // SAFETY: the typed marker supplies the exact signature and writable output.
        let create_result = unsafe {
            create(
                media_output,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                &mut swap_chain_output,
            )
        };
        assert_eq!(create_result, 0);
        assert_eq!(FACTORY_MEDIA_CREATE_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(swap_chain_output, swap_chain_pointer);
        assert!(!ptr::eq(swap_chain.vtable, original_swap_chain_vtable));

        let report = manager.close_and_drain(Duration::from_secs(1));
        assert!(report.drained);
        assert!(ptr::eq(factory.vtable, original_factory_vtable));
        assert!(ptr::eq(media.vtable, original_media_vtable));
        assert!(ptr::eq(swap_chain.vtable, original_swap_chain_vtable));
        unregister_route(manager_id, factory_pointer);
        unregister_route(manager_id, media_pointer);
        unregister_route(manager_id, swap_chain_pointer);
        FACTORY_MEDIA_POINTER.store(ptr::null_mut(), Ordering::Relaxed);
        // SAFETY: successful fake calls returned one owned reference each.
        unsafe {
            fake_release(media_output);
            fake_release(swap_chain_output);
        }
    }
}
