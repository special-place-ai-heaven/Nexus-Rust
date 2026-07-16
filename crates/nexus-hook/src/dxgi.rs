//! Typed DXGI interface layouts and interception points.
//!
//! Slot numbers live only in this module. Hooking code selects a named method
//! marker, and the trait system rejects methods that do not belong to the
//! prepared interface layout. Counts follow the inherited COM vtables in the
//! Windows SDK; notably, `IDXGISwapChain3::SetColorSpace1` is slot 38 and
//! `IDXGISwapChain3::ResizeBuffers1` is slot 39. The composition-surface-handle
//! creation method lives on the separate `IDXGIFactoryMedia` interface, not on
//! the inherited `IDXGIFactory2` vtable.

use std::ffi::c_void;

use crate::{ComInterfaceLayout, ComMethod};

macro_rules! define_layout {
    ($(#[$meta:meta])* $name:ident, $slot_count:literal, $interface_name:literal) => {
        $(#[$meta])*
        pub enum $name {}

        // SAFETY: the count is the complete inherited Windows SDK vtable for
        // the named interface, whose first three entries are IUnknown.
        unsafe impl ComInterfaceLayout for $name {
            const NAME: &'static str = $interface_name;
            const SLOT_COUNT: usize = $slot_count;
        }
    };
}

macro_rules! define_method {
    (
        $(#[$meta:meta])*
        $method:ident, $function:ident = $function_type:ty,
        $index:literal, $method_name:literal;
        $($layout:ty),+ $(,)?
    ) => {
        $(#[$meta])*
        pub struct $method;

        #[doc = concat!("Raw function signature for [`", stringify!($method), "`].")]
        pub type $function = $function_type;

        $(
            // SAFETY: the Windows SDK declares this method at the given slot
            // with the exact system-ABI signature for this inherited layout.
            unsafe impl ComMethod<$layout> for $method {
                type Function = $function;

                const INDEX: usize = $index;
                const NAME: &'static str = $method_name;
            }
        )+
    };
}

define_layout!(
    /// Complete `IDXGIFactory` interface layout.
    DxgiFactory,
    12,
    "IDXGIFactory"
);
define_layout!(
    /// Complete `IDXGIFactory1` interface layout.
    DxgiFactory1,
    14,
    "IDXGIFactory1"
);
define_layout!(
    /// Complete `IDXGIFactory2` interface layout.
    DxgiFactory2,
    25,
    "IDXGIFactory2"
);
define_layout!(
    /// Complete `IDXGIFactory3` interface layout.
    DxgiFactory3,
    26,
    "IDXGIFactory3"
);
define_layout!(
    /// Complete `IDXGIFactory4` interface layout.
    DxgiFactory4,
    28,
    "IDXGIFactory4"
);
define_layout!(
    /// Complete `IDXGIFactory5` interface layout.
    DxgiFactory5,
    29,
    "IDXGIFactory5"
);
define_layout!(
    /// Complete `IDXGIFactory6` interface layout.
    DxgiFactory6,
    30,
    "IDXGIFactory6"
);
define_layout!(
    /// Complete `IDXGIFactory7` interface layout.
    DxgiFactory7,
    32,
    "IDXGIFactory7"
);
define_layout!(
    /// Complete `IDXGIFactoryMedia` interface layout.
    ///
    /// Unlike the numbered factory interfaces, this interface derives directly
    /// from `IUnknown` and is obtained through `QueryInterface`.
    DxgiFactoryMedia,
    5,
    "IDXGIFactoryMedia"
);

define_layout!(
    /// Complete `IDXGISwapChain` interface layout.
    DxgiSwapChain,
    18,
    "IDXGISwapChain"
);
define_layout!(
    /// Complete `IDXGISwapChain1` interface layout.
    DxgiSwapChain1,
    29,
    "IDXGISwapChain1"
);
define_layout!(
    /// Complete `IDXGISwapChain2` interface layout.
    DxgiSwapChain2,
    36,
    "IDXGISwapChain2"
);
define_layout!(
    /// Complete `IDXGISwapChain3` interface layout.
    DxgiSwapChain3,
    40,
    "IDXGISwapChain3"
);
define_layout!(
    /// Complete `IDXGISwapChain4` interface layout.
    DxgiSwapChain4,
    41,
    "IDXGISwapChain4"
);

define_method!(
    /// Typed marker for `IDXGIFactory::CreateSwapChain`.
    CreateSwapChain,
    CreateSwapChainFn = unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        description: *const c_void,
        swap_chain: *mut *mut c_void,
    ) -> i32,
    10,
    "IDXGIFactory::CreateSwapChain";
    DxgiFactory,
    DxgiFactory1,
    DxgiFactory2,
    DxgiFactory3,
    DxgiFactory4,
    DxgiFactory5,
    DxgiFactory6,
    DxgiFactory7,
);

define_method!(
    /// Typed marker for `IDXGIFactory2::CreateSwapChainForHwnd`.
    CreateSwapChainForHwnd,
    CreateSwapChainForHwndFn = unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        window: *mut c_void,
        description: *const c_void,
        fullscreen_description: *const c_void,
        restrict_to_output: *mut c_void,
        swap_chain: *mut *mut c_void,
    ) -> i32,
    15,
    "IDXGIFactory2::CreateSwapChainForHwnd";
    DxgiFactory2,
    DxgiFactory3,
    DxgiFactory4,
    DxgiFactory5,
    DxgiFactory6,
    DxgiFactory7,
);

define_method!(
    /// Typed marker for `IDXGIFactory2::CreateSwapChainForCoreWindow`.
    CreateSwapChainForCoreWindow,
    CreateSwapChainForCoreWindowFn = unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        window: *mut c_void,
        description: *const c_void,
        restrict_to_output: *mut c_void,
        swap_chain: *mut *mut c_void,
    ) -> i32,
    16,
    "IDXGIFactory2::CreateSwapChainForCoreWindow";
    DxgiFactory2,
    DxgiFactory3,
    DxgiFactory4,
    DxgiFactory5,
    DxgiFactory6,
    DxgiFactory7,
);

define_method!(
    /// Typed marker for `IDXGIFactory2::CreateSwapChainForComposition`.
    CreateSwapChainForComposition,
    CreateSwapChainForCompositionFn = unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        description: *const c_void,
        restrict_to_output: *mut c_void,
        swap_chain: *mut *mut c_void,
    ) -> i32,
    24,
    "IDXGIFactory2::CreateSwapChainForComposition";
    DxgiFactory2,
    DxgiFactory3,
    DxgiFactory4,
    DxgiFactory5,
    DxgiFactory6,
    DxgiFactory7,
);

define_method!(
    /// Typed marker for `IDXGIFactoryMedia::CreateSwapChainForCompositionSurfaceHandle`.
    CreateSwapChainForCompositionSurfaceHandle,
    CreateSwapChainForCompositionSurfaceHandleFn = unsafe extern "system" fn(
        this: *mut c_void,
        device: *mut c_void,
        surface: *mut c_void,
        description: *const c_void,
        restrict_to_output: *mut c_void,
        swap_chain: *mut *mut c_void,
    ) -> i32,
    3,
    "IDXGIFactoryMedia::CreateSwapChainForCompositionSurfaceHandle";
    DxgiFactoryMedia,
);

define_method!(
    /// Typed marker for `IDXGISwapChain::Present`.
    Present,
    PresentFn = unsafe extern "system" fn(
        this: *mut c_void,
        sync_interval: u32,
        flags: u32,
    ) -> i32,
    8,
    "IDXGISwapChain::Present";
    DxgiSwapChain,
    DxgiSwapChain1,
    DxgiSwapChain2,
    DxgiSwapChain3,
    DxgiSwapChain4,
);

define_method!(
    /// Typed marker for `IDXGISwapChain::ResizeBuffers`.
    ResizeBuffers,
    ResizeBuffersFn = unsafe extern "system" fn(
        this: *mut c_void,
        buffer_count: u32,
        width: u32,
        height: u32,
        format: i32,
        flags: u32,
    ) -> i32,
    13,
    "IDXGISwapChain::ResizeBuffers";
    DxgiSwapChain,
    DxgiSwapChain1,
    DxgiSwapChain2,
    DxgiSwapChain3,
    DxgiSwapChain4,
);

define_method!(
    /// Typed marker for `IDXGISwapChain1::Present1`.
    Present1,
    Present1Fn = unsafe extern "system" fn(
        this: *mut c_void,
        sync_interval: u32,
        flags: u32,
        present_parameters: *const c_void,
    ) -> i32,
    22,
    "IDXGISwapChain1::Present1";
    DxgiSwapChain1,
    DxgiSwapChain2,
    DxgiSwapChain3,
    DxgiSwapChain4,
);

define_method!(
    /// Typed marker for `IDXGISwapChain3::SetColorSpace1` (slot 38).
    SetColorSpace1,
    SetColorSpace1Fn = unsafe extern "system" fn(
        this: *mut c_void,
        color_space: i32,
    ) -> i32,
    38,
    "IDXGISwapChain3::SetColorSpace1";
    DxgiSwapChain3,
    DxgiSwapChain4,
);

define_method!(
    /// Typed marker for `IDXGISwapChain3::ResizeBuffers1` (slot 39).
    ResizeBuffers1,
    ResizeBuffers1Fn = unsafe extern "system" fn(
        this: *mut c_void,
        buffer_count: u32,
        width: u32,
        height: u32,
        format: i32,
        flags: u32,
        creation_node_mask: *const u32,
        present_queue: *const *mut c_void,
    ) -> i32,
    39,
    "IDXGISwapChain3::ResizeBuffers1";
    DxgiSwapChain3,
    DxgiSwapChain4,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_layout_counts_cover_named_slots() {
        const {
            assert!(<DxgiFactory2 as ComInterfaceLayout>::SLOT_COUNT > 24);
            assert!(<DxgiFactoryMedia as ComInterfaceLayout>::SLOT_COUNT > 3);
            assert!(<DxgiSwapChain1 as ComInterfaceLayout>::SLOT_COUNT > 22);
            assert!(<DxgiSwapChain3 as ComInterfaceLayout>::SLOT_COUNT > 38);
            assert!(<DxgiSwapChain3 as ComInterfaceLayout>::SLOT_COUNT > 39);
        }
    }

    #[test]
    fn color_space_and_media_creation_use_sdk_slots() {
        assert_eq!(<SetColorSpace1 as ComMethod<DxgiSwapChain3>>::INDEX, 38);
        assert_eq!(
            <CreateSwapChainForCompositionSurfaceHandle as ComMethod<DxgiFactoryMedia>>::INDEX,
            3
        );
    }

    #[test]
    fn resize_buffers_one_uses_the_sdk_slot() {
        assert_eq!(<ResizeBuffers1 as ComMethod<DxgiSwapChain3>>::INDEX, 39);
    }
}
