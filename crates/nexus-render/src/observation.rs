//! Native-handle-free descriptions of swap chains seen by the platform layer.

/// Stable identity assigned to one native swap-chain object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SwapChainId(u64);

impl SwapChainId {
    /// Creates an identity from the platform layer's monotonically assigned value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying stable value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Opaque Win32 window identity without importing Windows bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hwnd(usize);

impl Hwnd {
    /// Creates a window identity from the platform-provided handle value.
    #[must_use]
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the opaque handle value.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

/// Stable identity assigned to one native graphics device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(u64);

impl DeviceId {
    /// Creates a device identity from a platform-assigned value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying stable value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The two-part Windows adapter locally unique identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterLuid {
    low_part: u32,
    high_part: i32,
}

impl AdapterLuid {
    /// Creates an adapter LUID from the native low and high parts.
    #[must_use]
    pub const fn new(low_part: u32, high_part: i32) -> Self {
        Self {
            low_part,
            high_part,
        }
    }

    /// Creates an adapter LUID from its packed signed representation.
    #[must_use]
    pub const fn from_i64(value: i64) -> Self {
        Self {
            low_part: value as u32,
            high_part: (value >> 32) as i32,
        }
    }

    /// Returns the native low part.
    #[must_use]
    pub const fn low_part(self) -> u32 {
        self.low_part
    }

    /// Returns the native high part.
    #[must_use]
    pub const fn high_part(self) -> i32 {
        self.high_part
    }

    /// Returns the packed signed representation.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        ((self.high_part as i64) << 32) | self.low_part as i64
    }
}

/// Pixel dimensions of a swap-chain back buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Extent2D {
    /// Back-buffer width in pixels.
    pub width: u32,
    /// Back-buffer height in pixels.
    pub height: u32,
}

impl Extent2D {
    /// Creates a pixel extent.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Returns whether either dimension is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Returns the area without overflowing 32-bit dimensions.
    #[must_use]
    pub const fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Render-target format after translation from the native DXGI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SurfaceFormat {
    /// Eight-bit RGBA in linear UNORM space.
    Rgba8Unorm,
    /// Eight-bit RGBA with sRGB conversion.
    Rgba8UnormSrgb,
    /// Eight-bit BGRA in linear UNORM space.
    Bgra8Unorm,
    /// Eight-bit BGRA with sRGB conversion.
    Bgra8UnormSrgb,
    /// Ten-bit RGB plus two-bit alpha, commonly used for HDR10.
    Rgb10A2Unorm,
    /// Sixteen-bit floating-point RGBA, commonly used for scRGB.
    Rgba16Float,
    /// A native value unknown to this version of the policy crate.
    Other(u32),
}

impl SurfaceFormat {
    /// Returns whether the format can carry the known HDR color spaces.
    #[must_use]
    pub const fn is_hdr_capable(self) -> bool {
        matches!(self, Self::Rgb10A2Unorm | Self::Rgba16Float)
    }
}

/// Swap-chain color space after translation from the native DXGI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColorSpace {
    /// Standard dynamic range sRGB/Rec.709 output.
    Srgb,
    /// Linear extended-range scRGB output.
    ScRgbLinear,
    /// PQ-encoded Rec.2020 HDR10 output.
    Hdr10Pq,
    /// A native value unknown to this version of the policy crate.
    Other(u32),
}

impl ColorSpace {
    /// Returns whether the color space represents high dynamic range output.
    #[must_use]
    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::ScRgbLinear | Self::Hdr10Pq)
    }
}

/// Native presentation entry point observed for the current sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PresentMethod {
    /// The original `IDXGISwapChain::Present` method.
    Present,
    /// The `IDXGISwapChain1::Present1` method.
    Present1,
}

/// Activity signals collected without making a render-policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    /// Total successful or attempted presentations observed for this object.
    pub present_count: u64,
    /// Global monotonic presentation sequence of its most recent presentation.
    pub last_present_sequence: u64,
    /// Number of adjacent observation cycles in which this object presented.
    pub consecutive_present_cycles: u32,
    /// Whether the owning window is currently visible.
    pub window_visible: bool,
    /// Whether the owning window is the foreground window.
    pub foreground: bool,
    /// Whether native presentation currently reports the object as occluded.
    pub occluded: bool,
}

impl Activity {
    /// Creates active, visible presentation metrics.
    #[must_use]
    pub const fn active(
        present_count: u64,
        last_present_sequence: u64,
        consecutive_present_cycles: u32,
    ) -> Self {
        Self {
            present_count,
            last_present_sequence,
            consecutive_present_cycles,
            window_visible: true,
            foreground: false,
            occluded: false,
        }
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            present_count: 0,
            last_present_sequence: 0,
            consecutive_present_cycles: 0,
            window_visible: true,
            foreground: false,
            occluded: false,
        }
    }
}

/// One complete, immutable sample of a native swap chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapChainObservation {
    /// Stable identity of the native swap-chain object.
    pub id: SwapChainId,
    /// Owning output window, if the native object has one.
    pub hwnd: Option<Hwnd>,
    /// Stable identity of the graphics device.
    pub device: DeviceId,
    /// Adapter on which the graphics device was created.
    pub adapter_luid: AdapterLuid,
    /// Current back-buffer format.
    pub format: SurfaceFormat,
    /// Current output color space.
    pub color_space: ColorSpace,
    /// Current back-buffer dimensions.
    pub size: Extent2D,
    /// Presentation entry point used by this sample.
    pub present_method: PresentMethod,
    /// Recent activity and window signals.
    pub activity: Activity,
}

impl SwapChainObservation {
    /// Returns whether both the format and color space represent known HDR output.
    #[must_use]
    pub const fn is_hdr_output(&self) -> bool {
        self.format.is_hdr_capable() && self.color_space.is_hdr()
    }
}

#[cfg(test)]
mod tests {
    use super::{AdapterLuid, ColorSpace, SurfaceFormat};

    #[test]
    fn adapter_luid_round_trips_signed_packed_value() {
        let packed = -0x0123_4567_8765_4321_i64;
        let luid = AdapterLuid::from_i64(packed);

        assert_eq!(luid.as_i64(), packed);
    }

    #[test]
    fn only_known_hdr_formats_and_spaces_report_hdr() {
        assert!(SurfaceFormat::Rgb10A2Unorm.is_hdr_capable());
        assert!(SurfaceFormat::Rgba16Float.is_hdr_capable());
        assert!(!SurfaceFormat::Bgra8Unorm.is_hdr_capable());
        assert!(ColorSpace::Hdr10Pq.is_hdr());
        assert!(ColorSpace::ScRgbLinear.is_hdr());
        assert!(!ColorSpace::Srgb.is_hdr());
    }
}
