//! Bounded, owner-aware texture loading for the Nexus addon ABI.
//!
//! The service keeps source acquisition and image decoding off the render
//! thread. GPU upload and addon callback delivery happen only when the host
//! calls [`TextureService::advance`].

#![deny(unsafe_code)]

mod backend;
mod config;
mod decoder;
mod error;
mod io;
mod overrides;
mod queue;
mod resource;
mod service;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_resource;

pub use backend::{
    BackendFailure, DecodeLimits, DecodedImage, Downloader, GpuBackend, GpuTexture, ImageDecoder,
    NoDownloader, OverrideProvider, ResourceProvider,
};
pub use config::TextureConfig;
pub use decoder::ImageRsDecoder;
pub use error::{ConfigError, QueueKind, TextureError};
pub use overrides::{DirectoryOverrides, NoOverrides};
pub use resource::{ModuleHandle, NoResources, WindowsResourceProvider};
pub use service::{
    AdvanceReport, DownloadTarget, LoadOptions, OwnerGeneration, RequestOutcome, RequestOwner,
    ServiceStats, TextureCallback, TextureCallbackEvent, TextureHandle, TextureService,
    TextureSource,
};
