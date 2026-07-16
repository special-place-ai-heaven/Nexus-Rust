//! Owned D3D11 texture creation for [`nexus_textures::GpuBackend`].
//!
//! Layout validation is platform-neutral. On Windows, [`D3d11GpuBackend`]
//! consumes one owned device reference, never acquires a device context, and
//! returns textures containing exactly one owned shader-resource-view reference.
//!
//! ```
//! use nexus_textures_d3d11::Rgba8UploadLayout;
//!
//! let layout = Rgba8UploadLayout::validate(2, 3, 24)?;
//! assert_eq!(layout.row_pitch(), 8);
//! assert_eq!(layout.byte_len(), 24);
//! # Ok::<(), nexus_textures_d3d11::D3d11TextureError>(())
//! ```

#![deny(unsafe_code)]

mod error;
mod layout;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_backend;

pub use error::D3d11TextureError;
pub use layout::{MAX_TEXTURE2D_DIMENSION, Rgba8UploadLayout};

#[cfg(windows)]
pub use windows_backend::{D3d11GpuBackend, D3d11GpuTexture};
