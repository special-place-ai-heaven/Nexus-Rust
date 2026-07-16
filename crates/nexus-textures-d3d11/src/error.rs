//! Closed failures for D3D11 texture validation and creation.

use thiserror::Error;

/// A closed, path-free, and pointer-free D3D11 texture backend failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum D3d11TextureError {
    /// Width or height was zero.
    #[error("RGBA8 texture dimensions must be nonzero")]
    ZeroDimension,
    /// A dimension exceeded the D3D11 Texture2D limit.
    #[error("RGBA8 texture dimensions exceed the D3D11 limit")]
    DimensionTooLarge,
    /// Row-pitch or total-byte arithmetic overflowed.
    #[error("RGBA8 texture layout overflowed")]
    LayoutOverflow,
    /// The supplied bytes did not exactly cover the declared image.
    #[error("RGBA8 texture byte length does not match its dimensions")]
    PixelLengthMismatch,
    /// The device was created with D3D11 single-threaded semantics.
    #[error("single-threaded D3D11 devices are not supported")]
    SingleThreadedDevice,
    /// D3D11 rejected immutable Texture2D creation.
    #[error("D3D11 texture creation failed with HRESULT {code:#010x}")]
    TextureCreation {
        /// Numeric HRESULT returned by D3D11.
        code: i32,
    },
    /// D3D11 reported success without returning a Texture2D interface.
    #[error("D3D11 texture creation returned no interface")]
    MissingTexture,
    /// D3D11 rejected shader-resource-view creation.
    #[error("D3D11 shader-resource-view creation failed with HRESULT {code:#010x}")]
    ShaderResourceViewCreation {
        /// Numeric HRESULT returned by D3D11.
        code: i32,
    },
    /// D3D11 reported success without returning an SRV interface.
    #[error("D3D11 shader-resource-view creation returned no interface")]
    MissingShaderResourceView,
}

impl D3d11TextureError {
    pub(crate) const fn is_input_rejection(self) -> bool {
        matches!(
            self,
            Self::ZeroDimension
                | Self::DimensionTooLarge
                | Self::LayoutOverflow
                | Self::PixelLengthMismatch
                | Self::SingleThreadedDevice
        )
    }
}
