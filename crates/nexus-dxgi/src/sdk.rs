use std::{ffi::c_void, ptr};

use nexus_hook::{QueryInterfaceFn, ReleaseFn};
use nexus_render::{AdapterLuid, ColorSpace, Extent2D, SurfaceFormat};
use windows::{
    Win32::{
        Foundation::{DXGI_STATUS_OCCLUDED, E_NOINTERFACE, LUID},
        Graphics::Dxgi::{
            Common::{
                DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT,
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
                DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
            },
            DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, IDXGIAdapter, IDXGIDevice,
            IDXGIFactory, IDXGIFactory1, IDXGIFactory2, IDXGIFactory3, IDXGIFactory4,
            IDXGIFactory5, IDXGIFactory6, IDXGIFactory7, IDXGIFactoryMedia, IDXGISwapChain,
            IDXGISwapChain1, IDXGISwapChain2, IDXGISwapChain3, IDXGISwapChain4,
        },
    },
    core::{IUnknown, Interface},
};
use windows_sys::{
    Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible},
    core::{GUID, IUnknown_Vtbl},
};

use crate::{FactoryInterface, HResultDisposition, SwapChainInterface};

/// Sentinel retained by policy until a successful `SetColorSpace1` is observed.
pub(crate) const UNKNOWN_COLOR_SPACE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeMetadata {
    pub(crate) hwnd: Option<usize>,
    pub(crate) device_identity: usize,
    pub(crate) adapter_luid: AdapterLuid,
    pub(crate) size: Extent2D,
    pub(crate) format: SurfaceFormat,
    pub(crate) window_visible: bool,
    pub(crate) foreground: bool,
}

/// One owned reference returned by `QueryInterface`.
pub(crate) struct QueriedInterface {
    pointer: *mut c_void,
    release: ReleaseFn,
}

impl QueriedInterface {
    pub(crate) const fn pointer(&self) -> *mut c_void {
        self.pointer
    }
}

impl Drop for QueriedInterface {
    fn drop(&mut self) {
        // SAFETY: construction records the Release function from the same live
        // interface reference returned by a successful QueryInterface call.
        unsafe { (self.release)(self.pointer) };
    }
}

/// Returns the generated Windows SDK IID for a factory layout as a sys GUID.
#[must_use]
pub fn factory_iid(interface: FactoryInterface) -> GUID {
    match interface {
        FactoryInterface::Base => sys_guid::<IDXGIFactory>(),
        FactoryInterface::V1 => sys_guid::<IDXGIFactory1>(),
        FactoryInterface::V2 => sys_guid::<IDXGIFactory2>(),
        FactoryInterface::V3 => sys_guid::<IDXGIFactory3>(),
        FactoryInterface::V4 => sys_guid::<IDXGIFactory4>(),
        FactoryInterface::V5 => sys_guid::<IDXGIFactory5>(),
        FactoryInterface::V6 => sys_guid::<IDXGIFactory6>(),
        FactoryInterface::V7 => sys_guid::<IDXGIFactory7>(),
        FactoryInterface::Media => sys_guid::<IDXGIFactoryMedia>(),
    }
}

/// Returns the generated Windows SDK IID for a swap-chain layout as a sys GUID.
#[must_use]
pub fn swap_chain_iid(interface: SwapChainInterface) -> GUID {
    match interface {
        SwapChainInterface::Base => sys_guid::<IDXGISwapChain>(),
        SwapChainInterface::V1 => sys_guid::<IDXGISwapChain1>(),
        SwapChainInterface::V2 => sys_guid::<IDXGISwapChain2>(),
        SwapChainInterface::V3 => sys_guid::<IDXGISwapChain3>(),
        SwapChainInterface::V4 => sys_guid::<IDXGISwapChain4>(),
    }
}

pub(crate) fn factory_interface(iid: &GUID) -> Option<FactoryInterface> {
    [
        FactoryInterface::Media,
        FactoryInterface::V7,
        FactoryInterface::V6,
        FactoryInterface::V5,
        FactoryInterface::V4,
        FactoryInterface::V3,
        FactoryInterface::V2,
        FactoryInterface::V1,
        FactoryInterface::Base,
    ]
    .into_iter()
    .find(|interface| guid_eq(iid, &factory_iid(*interface)))
}

pub(crate) fn swap_chain_interface(iid: &GUID) -> Option<SwapChainInterface> {
    [
        SwapChainInterface::V4,
        SwapChainInterface::V3,
        SwapChainInterface::V2,
        SwapChainInterface::V1,
        SwapChainInterface::Base,
    ]
    .into_iter()
    .find(|interface| guid_eq(iid, &swap_chain_iid(*interface)))
}

pub(crate) unsafe fn highest_factory(
    pointer: *mut c_void,
    query: QueryInterfaceFn,
) -> Option<(FactoryInterface, QueriedInterface)> {
    // FactoryMedia is not in this list because it is an independent
    // IUnknown-derived layout, not a higher numbered factory revision.
    for interface in [
        FactoryInterface::V7,
        FactoryInterface::V6,
        FactoryInterface::V5,
        FactoryInterface::V4,
        FactoryInterface::V3,
        FactoryInterface::V2,
        FactoryInterface::V1,
        FactoryInterface::Base,
    ] {
        // SAFETY: the caller supplies a live factory pointer and its original
        // QueryInterface implementation. Each successful result is owned.
        if let Ok(queried) = unsafe { query_interface(pointer, query, &factory_iid(interface)) } {
            return Some((interface, queried));
        }
    }
    None
}

pub(crate) unsafe fn highest_swap_chain(
    pointer: *mut c_void,
    query: QueryInterfaceFn,
) -> Option<(SwapChainInterface, QueriedInterface)> {
    for interface in [
        SwapChainInterface::V4,
        SwapChainInterface::V3,
        SwapChainInterface::V2,
        SwapChainInterface::V1,
        SwapChainInterface::Base,
    ] {
        // SAFETY: the caller supplies a live swap-chain pointer and its
        // original QueryInterface implementation. Successful results are owned.
        if let Ok(queried) = unsafe { query_interface(pointer, query, &swap_chain_iid(interface)) }
        {
            return Some((interface, queried));
        }
    }
    None
}

pub(crate) unsafe fn original_iunknown_methods(
    pointer: *mut c_void,
) -> Option<(QueryInterfaceFn, ReleaseFn)> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller promises a live COM interface. Every COM interface
    // begins with a pointer to an IUnknown-compatible vtable.
    let vtable = unsafe { pointer.cast::<*const IUnknown_Vtbl>().read() };
    // SAFETY: the preceding read obtained the IUnknown-compatible vtable pointer.
    let vtable = unsafe { vtable.as_ref() }?;
    // SAFETY: nexus-hook erases the IID pointee to c_void, but the system ABI,
    // pointer representation, and every other parameter are identical to the
    // SDK IUnknown declaration.
    let query = unsafe {
        std::mem::transmute::<
            unsafe extern "system" fn(
                *mut c_void,
                *const windows_sys::core::GUID,
                *mut *mut c_void,
            ) -> i32,
            QueryInterfaceFn,
        >(vtable.QueryInterface)
    };
    Some((query, vtable.Release))
}

pub(crate) unsafe fn query_interface(
    pointer: *mut c_void,
    query: QueryInterfaceFn,
    iid: &GUID,
) -> Result<QueriedInterface, i32> {
    let mut output = ptr::null_mut();
    // SAFETY: the caller owns a live reference and supplies its original
    // QueryInterface function. `output` is writable for one interface pointer.
    let result = unsafe { query(pointer, (iid as *const GUID).cast::<c_void>(), &mut output) };
    if result < 0 || output.is_null() {
        return Err(if result < 0 { result } else { E_NOINTERFACE.0 });
    }
    // SAFETY: a successful QueryInterface result is itself a live COM object.
    let (_, release) = unsafe { original_iunknown_methods(output) }.ok_or(E_NOINTERFACE.0)?;
    Ok(QueriedInterface {
        pointer: output,
        release,
    })
}

pub(crate) unsafe fn inspect_swap_chain(
    pointer: *mut c_void,
    interface: SwapChainInterface,
) -> Result<NativeMetadata, i32> {
    // SAFETY: the caller ties `pointer` to a live hook guard and the base DXGI
    // swap-chain layout is inherited by every supported version.
    let swap_chain =
        unsafe { IDXGISwapChain::from_raw_borrowed(&pointer) }.ok_or(E_NOINTERFACE.0)?;

    let (size, format, base_window) = if interface >= SwapChainInterface::V1 {
        // SAFETY: the version classification is obtained from a successful SDK
        // IID query for this concrete pointer.
        let swap_chain1 =
            unsafe { IDXGISwapChain1::from_raw_borrowed(&pointer) }.ok_or(E_NOINTERFACE.0)?;
        // SAFETY: `swap_chain1` borrows the live, classified interface pointer.
        let description = unsafe { swap_chain1.GetDesc1() }.map_err(error_code)?;
        (
            Extent2D::new(description.Width, description.Height),
            surface_format(description.Format),
            None,
        )
    } else {
        // SAFETY: the pointer implements the base interface by classification.
        let description = unsafe { swap_chain.GetDesc() }.map_err(error_code)?;
        (
            Extent2D::new(description.BufferDesc.Width, description.BufferDesc.Height),
            surface_format(description.BufferDesc.Format),
            hwnd_value(description.OutputWindow.0),
        )
    };

    let hwnd = if interface >= SwapChainInterface::V1 {
        // SAFETY: guarded by the classified inherited interface version.
        let swap_chain1 =
            unsafe { IDXGISwapChain1::from_raw_borrowed(&pointer) }.ok_or(E_NOINTERFACE.0)?;
        // SAFETY: `swap_chain1` borrows the live, classified interface pointer.
        unsafe { swap_chain1.GetHwnd() }
            .ok()
            .and_then(|window| hwnd_value(window.0))
            .or(base_window)
    } else {
        base_window
    };

    // SAFETY: GetDevice is an inherited named SDK method and the returned
    // wrapper owns its reference.
    let device = unsafe { swap_chain.GetDevice::<IDXGIDevice>() }.map_err(error_code)?;
    let identity = device.cast::<IUnknown>().map_err(error_code)?;
    let device_identity = identity.as_raw() as usize;
    // SAFETY: the generated IDXGIDevice wrapper owns a live native reference.
    let adapter: IDXGIAdapter = unsafe { device.GetAdapter() }.map_err(error_code)?;
    // SAFETY: the generated adapter wrapper owns a live native reference.
    let adapter_description = unsafe { adapter.GetDesc() }.map_err(error_code)?;
    let adapter_luid = adapter_luid(adapter_description.AdapterLuid);

    let window_visible = hwnd.is_none_or(|window| {
        // SAFETY: this is an opaque HWND value returned by DXGI. Win32 accepts
        // stale handles and reports false rather than dereferencing them.
        unsafe { IsWindowVisible(window as *mut c_void) != 0 }
    });
    let foreground = hwnd.is_some_and(|window| {
        // SAFETY: GetForegroundWindow has no preconditions.
        unsafe { GetForegroundWindow() as usize == window }
    });

    Ok(NativeMetadata {
        hwnd,
        device_identity,
        adapter_luid,
        size,
        format,
        window_visible,
        foreground,
    })
}

pub(crate) const fn surface_format(format: DXGI_FORMAT) -> SurfaceFormat {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => SurfaceFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => SurfaceFormat::Rgba8UnormSrgb,
        DXGI_FORMAT_B8G8R8A8_UNORM => SurfaceFormat::Bgra8Unorm,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => SurfaceFormat::Bgra8UnormSrgb,
        DXGI_FORMAT_R10G10B10A2_UNORM => SurfaceFormat::Rgb10A2Unorm,
        DXGI_FORMAT_R16G16B16A16_FLOAT => SurfaceFormat::Rgba16Float,
        other => SurfaceFormat::Other(other.0 as u32),
    }
}

pub(crate) const fn raw_surface_format(format: i32) -> SurfaceFormat {
    surface_format(DXGI_FORMAT(format))
}

pub(crate) const fn color_space(value: i32) -> ColorSpace {
    match DXGI_COLOR_SPACE_TYPE(value) {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => ColorSpace::Srgb,
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => ColorSpace::ScRgbLinear,
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => ColorSpace::Hdr10Pq,
        other => ColorSpace::Other(other.0 as u32),
    }
}

pub(crate) const fn hresult_disposition(result: i32) -> HResultDisposition {
    if result == DXGI_STATUS_OCCLUDED.0 {
        HResultDisposition::Occluded
    } else if result == DXGI_ERROR_DEVICE_REMOVED.0 {
        HResultDisposition::DeviceRemoved
    } else if result == DXGI_ERROR_DEVICE_RESET.0 {
        HResultDisposition::DeviceReset
    } else if result >= 0 {
        HResultDisposition::Success
    } else {
        HResultDisposition::Other(result)
    }
}

fn sys_guid<T: Interface>() -> GUID {
    let iid = T::IID;
    GUID {
        data1: iid.data1,
        data2: iid.data2,
        data3: iid.data3,
        data4: iid.data4,
    }
}

fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

const fn adapter_luid(luid: LUID) -> AdapterLuid {
    AdapterLuid::new(luid.LowPart, luid.HighPart)
}

fn hwnd_value(window: *mut c_void) -> Option<usize> {
    (!window.is_null()).then_some(window as usize)
}

fn error_code(error: windows::core::Error) -> i32 {
    error.code().0
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::c_void,
        mem::{align_of, offset_of, size_of},
    };

    use nexus_hook::{
        ComInterfaceLayout, ComMethod,
        dxgi::{
            CreateSwapChainForCompositionSurfaceHandle, DxgiFactoryMedia, DxgiSwapChain3,
            SetColorSpace1,
        },
    };
    use windows::{
        Win32::{
            Foundation::HANDLE,
            Graphics::Dxgi::{DXGI_SWAP_CHAIN_DESC1, IDXGIFactoryMedia_Vtbl, IDXGISwapChain3_Vtbl},
        },
        core::HRESULT,
    };

    use super::*;

    type SdkSetColorSpace1Fn =
        unsafe extern "system" fn(*mut c_void, DXGI_COLOR_SPACE_TYPE) -> HRESULT;
    type SdkCreateForSurfaceHandleFn = unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        HANDLE,
        *const DXGI_SWAP_CHAIN_DESC1,
        *mut c_void,
        *mut *mut c_void,
    ) -> HRESULT;

    fn sdk_signatures_compile(swap_chain: &IDXGISwapChain3_Vtbl, factory: &IDXGIFactoryMedia_Vtbl) {
        let _: SdkSetColorSpace1Fn = swap_chain.SetColorSpace1;
        let _: SdkCreateForSurfaceHandleFn = factory.CreateSwapChainForCompositionSurfaceHandle;
    }

    #[test]
    fn generated_iids_round_trip_to_supported_layouts() {
        for interface in [
            FactoryInterface::Base,
            FactoryInterface::V1,
            FactoryInterface::V2,
            FactoryInterface::V3,
            FactoryInterface::V4,
            FactoryInterface::V5,
            FactoryInterface::V6,
            FactoryInterface::V7,
            FactoryInterface::Media,
        ] {
            assert_eq!(factory_interface(&factory_iid(interface)), Some(interface));
        }
        for interface in [
            SwapChainInterface::Base,
            SwapChainInterface::V1,
            SwapChainInterface::V2,
            SwapChainInterface::V3,
            SwapChainInterface::V4,
        ] {
            assert_eq!(
                swap_chain_interface(&swap_chain_iid(interface)),
                Some(interface)
            );
        }
    }

    #[test]
    fn unknown_color_space_is_not_an_sdr_value() {
        assert_ne!(UNKNOWN_COLOR_SPACE, 0);
        assert_eq!(
            nexus_render::ColorSpace::Other(UNKNOWN_COLOR_SPACE),
            nexus_render::ColorSpace::Other(u32::MAX)
        );
    }

    #[test]
    fn color_space_translation_retains_unknown_native_values() {
        assert_eq!(
            color_space(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709.0),
            ColorSpace::Srgb
        );
        assert_eq!(
            color_space(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709.0),
            ColorSpace::ScRgbLinear
        );
        assert_eq!(
            color_space(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0),
            ColorSpace::Hdr10Pq
        );
        assert_eq!(color_space(12_345), ColorSpace::Other(12_345));
        assert_eq!(color_space(-2), ColorSpace::Other(u32::MAX - 1));
    }

    #[test]
    fn hook_layouts_and_raw_signatures_match_windows_sdk() {
        let _: fn(&IDXGISwapChain3_Vtbl, &IDXGIFactoryMedia_Vtbl) = sdk_signatures_compile;

        assert_eq!(
            size_of::<IDXGISwapChain3_Vtbl>() / size_of::<usize>(),
            <DxgiSwapChain3 as ComInterfaceLayout>::SLOT_COUNT
        );
        assert_eq!(
            offset_of!(IDXGISwapChain3_Vtbl, SetColorSpace1) / size_of::<usize>(),
            <SetColorSpace1 as ComMethod<DxgiSwapChain3>>::INDEX
        );
        assert_eq!(
            size_of::<IDXGIFactoryMedia_Vtbl>() / size_of::<usize>(),
            <DxgiFactoryMedia as ComInterfaceLayout>::SLOT_COUNT
        );
        assert_eq!(
            offset_of!(
                IDXGIFactoryMedia_Vtbl,
                CreateSwapChainForCompositionSurfaceHandle
            ) / size_of::<usize>(),
            <CreateSwapChainForCompositionSurfaceHandle as ComMethod<DxgiFactoryMedia>>::INDEX
        );

        assert_eq!(size_of::<DXGI_COLOR_SPACE_TYPE>(), size_of::<i32>());
        assert_eq!(align_of::<DXGI_COLOR_SPACE_TYPE>(), align_of::<i32>());
        assert_eq!(size_of::<HANDLE>(), size_of::<*mut c_void>());
        assert_eq!(align_of::<HANDLE>(), align_of::<*mut c_void>());
        assert_eq!(size_of::<HRESULT>(), size_of::<i32>());
        assert_eq!(align_of::<HRESULT>(), align_of::<i32>());
    }

    #[test]
    fn format_translation_retains_unknown_native_values() {
        assert_eq!(raw_surface_format(12_345), SurfaceFormat::Other(12_345));
    }
}
