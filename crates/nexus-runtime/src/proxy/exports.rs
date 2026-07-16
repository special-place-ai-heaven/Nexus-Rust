#![allow(non_snake_case)]

use core::{ffi::c_void, ptr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use windows_sys::core::GUID;

use crate::{
    diagnostics::{report_proxy_failure, report_proxy_panic},
    dxgi,
    runtime::{self, ProxyFunction},
};

use super::{ModuleKind, ProxyModule, RecursionGuard};

const E_FAIL: i32 = 0x8000_4005_u32 as i32;
const D3D11_CREATE_DEVICE_DEBUG: u32 = 1 << 1;

fn contain_runtime_side_effect(operation: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(operation)).is_err() {
        report_proxy_panic();
    }
}

fn effective_debug_device_requested() -> bool {
    match catch_unwind(AssertUnwindSafe(runtime::debug_device_requested)) {
        Ok(requested) => requested,
        Err(_) => {
            report_proxy_panic();
            false
        }
    }
}

macro_rules! forward_export {
    (
        $kind:expr,
        $entry:expr,
        $name:literal,
        $name_nul:literal,
        $function_type:ty,
        $fallback:expr,
        ($($argument:expr),* $(,)?)
    ) => {{
        // Runtime startup is best-effort. It must never prevent the native
        // export from being resolved and called exactly once.
        contain_runtime_side_effect(|| runtime::initialize($entry));
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<_, ()> {
            let module = ProxyModule::get($kind).map_err(|error| {
                report_proxy_failure(error);
            })?;
            let system_only = RecursionGuard::is_active();
            // SAFETY: each invocation supplies the exact Windows SDK function
            // signature for this named export.
            let function = unsafe {
                module
                    .resolve::<$function_type>($name, $name_nul, system_only)
                    .map_err(|error| {
                        report_proxy_failure(&error);
                    })?
            };
            let _recursion = RecursionGuard::enter();
            // SAFETY: the outer exported function received these arguments
            // under the same ABI and forwards them without dereferencing.
            Ok(unsafe { function($($argument),*) })
        }));

        match result {
            Ok(Ok(value)) => value,
            Ok(Err(())) => $fallback,
            Err(_) => {
                report_proxy_panic();
                $fallback
            }
        }
    }};
}

/// Forwards `Direct3DCreate9` to the chainload or system D3D9 module.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Direct3DCreate9(sdk_version: u32) -> *mut c_void {
    type Function = unsafe extern "system" fn(u32) -> *mut c_void;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9Direct3dCreate9,
        "Direct3DCreate9",
        b"Direct3DCreate9\0",
        Function,
        ptr::null_mut(),
        (sdk_version)
    )
}

/// Forwards `Direct3DCreate9Ex` to the chainload or system D3D9 module.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Direct3DCreate9Ex(sdk_version: u32, d3d: *mut *mut c_void) -> i32 {
    type Function = unsafe extern "system" fn(u32, *mut *mut c_void) -> i32;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9Direct3dCreate9Ex,
        "Direct3DCreate9Ex",
        b"Direct3DCreate9Ex\0",
        Function,
        E_FAIL,
        (sdk_version, d3d)
    )
}

/// Forwards `D3DPERF_BeginEvent` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_BeginEvent(color: u32, name: *const u16) -> i32 {
    type Function = unsafe extern "system" fn(u32, *const u16) -> i32;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfBeginEvent,
        "D3DPERF_BeginEvent",
        b"D3DPERF_BeginEvent\0",
        Function,
        -1,
        (color, name)
    )
}

/// Forwards `D3DPERF_EndEvent` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_EndEvent() -> i32 {
    type Function = unsafe extern "system" fn() -> i32;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfEndEvent,
        "D3DPERF_EndEvent",
        b"D3DPERF_EndEvent\0",
        Function,
        -1,
        ()
    )
}

/// Forwards `D3DPERF_SetMarker` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_SetMarker(color: u32, name: *const u16) {
    type Function = unsafe extern "system" fn(u32, *const u16);
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfSetMarker,
        "D3DPERF_SetMarker",
        b"D3DPERF_SetMarker\0",
        Function,
        (),
        (color, name)
    );
}

/// Forwards `D3DPERF_SetRegion` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_SetRegion(color: u32, name: *const u16) {
    type Function = unsafe extern "system" fn(u32, *const u16);
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfSetRegion,
        "D3DPERF_SetRegion",
        b"D3DPERF_SetRegion\0",
        Function,
        (),
        (color, name)
    );
}

/// Forwards `D3DPERF_QueryRepeatFrame` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_QueryRepeatFrame() -> i32 {
    type Function = unsafe extern "system" fn() -> i32;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfQueryRepeatFrame,
        "D3DPERF_QueryRepeatFrame",
        b"D3DPERF_QueryRepeatFrame\0",
        Function,
        0,
        ()
    )
}

/// Forwards `D3DPERF_SetOptions` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_SetOptions(options: u32) {
    type Function = unsafe extern "system" fn(u32);
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfSetOptions,
        "D3DPERF_SetOptions",
        b"D3DPERF_SetOptions\0",
        Function,
        (),
        (options)
    );
}

/// Forwards `D3DPERF_GetStatus` to D3D9.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DPERF_GetStatus() -> u32 {
    type Function = unsafe extern "system" fn() -> u32;
    forward_export!(
        ModuleKind::D3d9,
        ProxyFunction::D3d9PerfGetStatus,
        "D3DPERF_GetStatus",
        b"D3DPERF_GetStatus\0",
        Function,
        0,
        ()
    )
}

/// Forwards `D3D11CreateDevice` while preserving Nexus's `-ggdev` behavior.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn D3D11CreateDevice(
    adapter: *mut c_void,
    driver_type: i32,
    software: *mut c_void,
    mut flags: u32,
    feature_levels: *const i32,
    feature_level_count: u32,
    sdk_version: u32,
    device: *mut *mut c_void,
    selected_feature_level: *mut i32,
    immediate_context: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(
        *mut c_void,
        i32,
        *mut c_void,
        u32,
        *const i32,
        u32,
        u32,
        *mut *mut c_void,
        *mut i32,
        *mut *mut c_void,
    ) -> i32;

    if effective_debug_device_requested() {
        flags |= D3D11_CREATE_DEVICE_DEBUG;
    }
    forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CreateDevice,
        "D3D11CreateDevice",
        b"D3D11CreateDevice\0",
        Function,
        E_FAIL,
        (
            adapter,
            driver_type,
            software,
            flags,
            feature_levels,
            feature_level_count,
            sdk_version,
            device,
            selected_feature_level,
            immediate_context
        )
    )
}

/// Forwards `D3D11CreateDeviceAndSwapChain` and preserves `-ggdev`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn D3D11CreateDeviceAndSwapChain(
    adapter: *mut c_void,
    driver_type: i32,
    software: *mut c_void,
    mut flags: u32,
    feature_levels: *const i32,
    feature_level_count: u32,
    sdk_version: u32,
    swap_chain_description: *const c_void,
    swap_chain: *mut *mut c_void,
    device: *mut *mut c_void,
    selected_feature_level: *mut i32,
    immediate_context: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(
        *mut c_void,
        i32,
        *mut c_void,
        u32,
        *const i32,
        u32,
        u32,
        *const c_void,
        *mut *mut c_void,
        *mut *mut c_void,
        *mut i32,
        *mut *mut c_void,
    ) -> i32;

    if effective_debug_device_requested() {
        flags |= D3D11_CREATE_DEVICE_DEBUG;
    }
    let result = forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CreateDeviceAndSwapChain,
        "D3D11CreateDeviceAndSwapChain",
        b"D3D11CreateDeviceAndSwapChain\0",
        Function,
        E_FAIL,
        (
            adapter,
            driver_type,
            software,
            flags,
            feature_levels,
            feature_level_count,
            sdk_version,
            swap_chain_description,
            swap_chain,
            device,
            selected_feature_level,
            immediate_context
        )
    );
    // SAFETY: a successful native call initialized the caller-owned output
    // slot and the returned COM reference remains live on return.
    unsafe { dxgi::after_swap_chain(result, swap_chain) };
    result
}

/// Forwards `D3D11CoreCreateDevice` to D3D11.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn D3D11CoreCreateDevice(
    factory: *mut c_void,
    adapter: *mut c_void,
    flags: u32,
    feature_levels: *const i32,
    feature_level_count: u32,
    device: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *const i32,
        u32,
        *mut *mut c_void,
    ) -> i32;
    forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CoreCreateDevice,
        "D3D11CoreCreateDevice",
        b"D3D11CoreCreateDevice\0",
        Function,
        E_FAIL,
        (
            factory,
            adapter,
            flags,
            feature_levels,
            feature_level_count,
            device
        )
    )
}

/// Forwards `D3D11CoreCreateLayeredDevice` to D3D11.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D11CoreCreateLayeredDevice(
    unknown0: *const c_void,
    unknown1: u32,
    unknown2: *const c_void,
    interface_id: *const GUID,
    object: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(
        *const c_void,
        u32,
        *const c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> i32;
    forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CoreCreateLayeredDevice,
        "D3D11CoreCreateLayeredDevice",
        b"D3D11CoreCreateLayeredDevice\0",
        Function,
        E_FAIL,
        (unknown0, unknown1, unknown2, interface_id, object)
    )
}

/// Forwards `D3D11CoreGetLayeredDeviceSize` to D3D11.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D11CoreGetLayeredDeviceSize(
    unknown0: *const c_void,
    unknown1: u32,
) -> usize {
    type Function = unsafe extern "system" fn(*const c_void, u32) -> usize;
    forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CoreGetLayeredDeviceSize,
        "D3D11CoreGetLayeredDeviceSize",
        b"D3D11CoreGetLayeredDeviceSize\0",
        Function,
        0,
        (unknown0, unknown1)
    )
}

/// Forwards `D3D11CoreRegisterLayers` to D3D11.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D11CoreRegisterLayers(unknown: *const c_void, count: u32) -> i32 {
    type Function = unsafe extern "system" fn(*const c_void, u32) -> i32;
    forward_export!(
        ModuleKind::D3d11,
        ProxyFunction::D3d11CoreRegisterLayers,
        "D3D11CoreRegisterLayers",
        b"D3D11CoreRegisterLayers\0",
        Function,
        E_FAIL,
        (unknown, count)
    )
}

/// Forwards `CreateDXGIFactory` to DXGI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory(
    interface_id: *const GUID,
    factory: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> i32;
    let result = forward_export!(
        ModuleKind::Dxgi,
        ProxyFunction::DxgiCreateFactory,
        "CreateDXGIFactory",
        b"CreateDXGIFactory\0",
        Function,
        E_FAIL,
        (interface_id, factory)
    );
    // SAFETY: a successful native call initialized the caller-owned output
    // slot for the requested interface.
    unsafe { dxgi::after_factory(result, interface_id, factory) };
    result
}

/// Forwards `CreateDXGIFactory1` to DXGI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory1(
    interface_id: *const GUID,
    factory: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> i32;
    let result = forward_export!(
        ModuleKind::Dxgi,
        ProxyFunction::DxgiCreateFactory1,
        "CreateDXGIFactory1",
        b"CreateDXGIFactory1\0",
        Function,
        E_FAIL,
        (interface_id, factory)
    );
    // SAFETY: a successful native call initialized the caller-owned output
    // slot for the requested interface.
    unsafe { dxgi::after_factory(result, interface_id, factory) };
    result
}

/// Forwards `CreateDXGIFactory2` to DXGI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory2(
    flags: u32,
    interface_id: *const GUID,
    factory: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> i32;
    let result = forward_export!(
        ModuleKind::Dxgi,
        ProxyFunction::DxgiCreateFactory2,
        "CreateDXGIFactory2",
        b"CreateDXGIFactory2\0",
        Function,
        E_FAIL,
        (flags, interface_id, factory)
    );
    // SAFETY: a successful native call initialized the caller-owned output
    // slot for the requested interface.
    unsafe { dxgi::after_factory(result, interface_id, factory) };
    result
}

/// Forwards `DXGIGetDebugInterface1` to DXGI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DXGIGetDebugInterface1(
    flags: u32,
    interface_id: *const GUID,
    debug: *mut *mut c_void,
) -> i32 {
    type Function = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> i32;
    forward_export!(
        ModuleKind::Dxgi,
        ProxyFunction::DxgiGetDebugInterface1,
        "DXGIGetDebugInterface1",
        b"DXGIGetDebugInterface1\0",
        Function,
        E_FAIL,
        (flags, interface_id, debug)
    )
}

/// Forwards `DXGIDeclareAdapterRemovalSupport` to DXGI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DXGIDeclareAdapterRemovalSupport() -> i32 {
    type Function = unsafe extern "system" fn() -> i32;
    forward_export!(
        ModuleKind::Dxgi,
        ProxyFunction::DxgiDeclareAdapterRemovalSupport,
        "DXGIDeclareAdapterRemovalSupport",
        b"DXGIDeclareAdapterRemovalSupport\0",
        Function,
        E_FAIL,
        ()
    )
}
