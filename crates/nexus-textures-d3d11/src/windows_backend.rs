//! Windows D3D11 implementation of the Nexus GPU texture traits.

use core::fmt;
use core::num::NonZeroUsize;

use nexus_textures::{BackendFailure, DecodedImage, GpuBackend, GpuTexture};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_IMMUTABLE, ID3D11Device, ID3D11ShaderResourceView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::{D3d11TextureError, Rgba8UploadLayout};

/// D3D11 texture backend which owns exactly one device interface reference.
///
/// The backend never requests, stores, or transfers an immediate device
/// context. Device creation with `D3D11_CREATE_DEVICE_SINGLETHREADED` is
/// rejected because [`GpuBackend`] is intentionally `Send + Sync`.
pub struct D3d11GpuBackend {
    device: ID3D11Device,
}

impl D3d11GpuBackend {
    /// Consumes one owned device reference after validating thread semantics.
    ///
    /// # Errors
    ///
    /// Returns [`D3d11TextureError::SingleThreadedDevice`] when the device was
    /// created with D3D11 single-threaded behavior.
    pub fn new(device: ID3D11Device) -> Result<Self, D3d11TextureError> {
        // SAFETY: `device` is a live, owned ID3D11Device interface.
        let flags = unsafe { device.GetCreationFlags() };
        if flags & D3D11_CREATE_DEVICE_SINGLETHREADED.0 != 0 {
            return Err(D3d11TextureError::SingleThreadedDevice);
        }
        Ok(Self { device })
    }

    /// Creates one immutable RGBA8 texture and returns its sole owned SRV.
    ///
    /// The temporary Texture2D reference is released before this method
    /// returns. The SRV retains the resource as required by COM.
    ///
    /// # Errors
    ///
    /// Returns a closed validation, Texture2D, or SRV creation failure.
    pub fn create_texture(
        &self,
        image: &DecodedImage,
    ) -> Result<D3d11GpuTexture, D3d11TextureError> {
        let layout = Rgba8UploadLayout::validate(image.width, image.height, image.rgba8.len())?;
        let descriptor = texture_descriptor(layout);
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: image.rgba8.as_ptr().cast(),
            SysMemPitch: layout.row_pitch(),
            SysMemSlicePitch: 0,
        };

        let mut texture = None;
        // SAFETY: the validated byte slice covers every row described by
        // `descriptor` and remains live for the synchronous D3D11 call.
        unsafe {
            self.device
                .CreateTexture2D(&descriptor, Some(&initial), Some(&mut texture))
        }
        .map_err(|error| D3d11TextureError::TextureCreation {
            code: error.code().0,
        })?;
        let texture = texture.ok_or(D3d11TextureError::MissingTexture)?;

        let mut view = None;
        // SAFETY: `texture` is a live RGBA8 Texture2D with the shader-resource
        // bind flag; a default SRV exactly covers its single mip and array item.
        unsafe {
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
        }
        .map_err(|error| D3d11TextureError::ShaderResourceViewCreation {
            code: error.code().0,
        })?;
        let view = view.ok_or(D3d11TextureError::MissingShaderResourceView)?;
        let address = NonZeroUsize::new(view.as_raw().addr())
            .ok_or(D3d11TextureError::MissingShaderResourceView)?;

        // Dropping this local releases the creator's Texture2D reference. The
        // SRV held below owns the resource lifetime from this point onward.
        drop(texture);
        Ok(D3d11GpuTexture {
            _view: view,
            address,
        })
    }
}

impl fmt::Debug for D3d11GpuBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("D3d11GpuBackend([device redacted])")
    }
}

impl GpuBackend for D3d11GpuBackend {
    fn create_rgba8(&self, image: &DecodedImage) -> Result<Box<dyn GpuTexture>, BackendFailure> {
        self.create_texture(image)
            .map(|texture| Box::new(texture) as Box<dyn GpuTexture>)
            .map_err(|error| {
                if error.is_input_rejection() {
                    BackendFailure::Rejected
                } else {
                    BackendFailure::Unavailable
                }
            })
    }
}

/// One owned `ID3D11ShaderResourceView` reference and its stable ABI address.
pub struct D3d11GpuTexture {
    _view: ID3D11ShaderResourceView,
    address: NonZeroUsize,
}

impl D3d11GpuTexture {
    /// Returns the stable SRV interface address stored in Nexus' texture ABI.
    #[must_use]
    pub const fn address(&self) -> NonZeroUsize {
        self.address
    }
}

impl GpuTexture for D3d11GpuTexture {
    fn srv_address(&self) -> NonZeroUsize {
        self.address
    }
}

impl fmt::Debug for D3d11GpuTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("D3d11GpuTexture([SRV address redacted])")
    }
}

fn texture_descriptor(layout: Rgba8UploadLayout) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: layout.width(),
        Height: layout.height(),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;

    use nexus_textures::{DecodedImage, GpuBackend, GpuTexture};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_SINGLETHREADED,
        D3D11_SDK_VERSION, D3D11_USAGE_IMMUTABLE, D3D11CreateDevice, ID3D11Device,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    use windows::core::IUnknown_Vtbl;

    use super::{D3d11GpuBackend, texture_descriptor};
    use crate::{D3d11TextureError, Rgba8UploadLayout};

    fn warp_device(flags: D3D11_CREATE_DEVICE_FLAG) -> windows::core::Result<ID3D11Device> {
        let mut device = None;
        // SAFETY: WARP creates a process-local software device. Optional adapter,
        // feature-level, and device-context outputs are intentionally absent.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }?;
        device.ok_or_else(|| {
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
        })
    }

    #[test]
    fn pure_texture_descriptor_is_immutable_rgba8_srv_only() {
        let layout = Rgba8UploadLayout::validate(3, 2, 24);
        let Ok(layout) = layout else {
            panic!("test layout must be valid");
        };
        let descriptor = texture_descriptor(layout);
        assert_eq!(descriptor.Width, 3);
        assert_eq!(descriptor.Height, 2);
        assert_eq!(descriptor.MipLevels, 1);
        assert_eq!(descriptor.ArraySize, 1);
        assert_eq!(descriptor.Format, DXGI_FORMAT_R8G8B8A8_UNORM);
        assert_eq!(descriptor.SampleDesc.Count, 1);
        assert_eq!(descriptor.SampleDesc.Quality, 0);
        assert_eq!(descriptor.Usage, D3D11_USAGE_IMMUTABLE);
        assert_eq!(descriptor.BindFlags, D3D11_BIND_SHADER_RESOURCE.0 as u32);
        assert_eq!(descriptor.CPUAccessFlags, 0);
        assert_eq!(descriptor.MiscFlags, 0);
    }

    #[test]
    fn single_threaded_device_is_rejected_before_backend_publication() {
        let device = warp_device(D3D11_CREATE_DEVICE_SINGLETHREADED);
        let Ok(device) = device else {
            panic!("Windows WARP device creation must be available");
        };
        assert_eq!(
            D3d11GpuBackend::new(device).map(|_backend| ()),
            Err(D3d11TextureError::SingleThreadedDevice)
        );
    }

    #[test]
    fn warp_upload_returns_one_stable_owned_srv_reference() {
        let device = warp_device(D3D11_CREATE_DEVICE_FLAG::default());
        let Ok(device) = device else {
            panic!("Windows WARP device creation must be available");
        };
        let backend = D3d11GpuBackend::new(device);
        let Ok(backend) = backend else {
            panic!("default WARP device must be thread-safe");
        };
        let image = DecodedImage {
            width: 2,
            height: 2,
            rgba8: vec![255; 16],
        };
        let texture = backend.create_texture(&image);
        let Ok(texture) = texture else {
            panic!("WARP must accept a validated RGBA8 upload");
        };
        let address = texture.address();
        assert_eq!(texture.srv_address(), address);
        assert_eq!(texture.srv_address(), address);

        // The concrete type contains the only owned SRV interface. A temporary
        // AddRef/Release pair therefore observes exactly one retained reference.
        let raw = address.get() as *mut c_void;
        // SAFETY: the stable address belongs to the live texture above and every
        // COM interface begins with a valid IUnknown vtable.
        let vtable = unsafe { *raw.cast::<*const IUnknown_Vtbl>() };
        // SAFETY: `raw` is the live interface matching this vtable.
        let after_add = unsafe { ((*vtable).AddRef)(raw) };
        // SAFETY: this balances the immediately preceding temporary AddRef.
        let after_release = unsafe { ((*vtable).Release)(raw) };
        assert_eq!(after_add, 2);
        assert_eq!(after_release, 1);

        drop(backend);
        assert_eq!(texture.srv_address(), address);
        let erased = Box::new(texture) as Box<dyn GpuTexture>;
        assert_eq!(erased.srv_address(), address);
        drop(erased);
    }

    #[test]
    fn trait_maps_validation_and_device_failures_to_closed_categories() {
        let device = warp_device(D3D11_CREATE_DEVICE_FLAG::default());
        let Ok(device) = device else {
            panic!("Windows WARP device creation must be available");
        };
        let backend = D3d11GpuBackend::new(device);
        let Ok(backend) = backend else {
            panic!("default WARP device must be thread-safe");
        };
        let invalid = DecodedImage {
            width: 1,
            height: 1,
            rgba8: Vec::new(),
        };
        assert!(matches!(
            backend.create_rgba8(&invalid),
            Err(nexus_textures::BackendFailure::Rejected)
        ));
        let debug = format!("{backend:?}");
        assert!(debug.contains("redacted"));
        assert!(!debug.contains("0x"));
    }
}
