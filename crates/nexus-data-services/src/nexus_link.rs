use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use core::ptr;
use std::sync::{Mutex, MutexGuard};

use nexus_abi::NexusLinkData;
use nexus_gw2::DerivedTelemetry;
use thiserror::Error;

use crate::{DataLinkService, DataServiceError, ResourceKind, ResourceLease};

/// Legacy DataLink identifier for the shared Nexus compatibility snapshot.
pub const DL_NEXUS_LINK: &str = "DL_NEXUS_LINK";

/// Closed Quick Access position values used by the legacy ABI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(i32)]
pub enum QuickAccessPosition {
    /// Extend the game-native quick-access row.
    #[default]
    Extend = 0,
    /// Place the row under the game-native quick-access row.
    Under = 1,
    /// Place the row along the bottom edge.
    Bottom = 2,
    /// Use the configured custom position.
    Custom = 3,
}

/// Immutable font-address snapshot for `DL_NEXUS_LINK`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FontSnapshot {
    regular: usize,
    large: usize,
    ui: usize,
}

impl FontSnapshot {
    /// Captures three opaque `ImFont*` addresses as non-dereferenced tokens.
    #[must_use]
    pub const fn from_addresses(regular: usize, large: usize, ui: usize) -> Self {
        Self { regular, large, ui }
    }
}

/// Immutable validated render fields for one NexusLink publication.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderSnapshot {
    width: u32,
    height: u32,
    scaling: f32,
    fonts: FontSnapshot,
}

impl RenderSnapshot {
    /// Creates a render snapshot with a finite positive UI scale.
    pub fn new(
        width: u32,
        height: u32,
        scaling: f32,
        fonts: FontSnapshot,
    ) -> Result<Self, NexusLinkSnapshotError> {
        if !scaling.is_finite() || scaling <= 0.0 {
            return Err(NexusLinkSnapshotError::InvalidScaling);
        }
        Ok(Self {
            width,
            height,
            scaling,
            fonts,
        })
    }
}

/// Immutable validated Quick Access fields for one NexusLink publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuickAccessSnapshot {
    active_icons: i32,
    position: QuickAccessPosition,
    is_vertical: bool,
}

impl QuickAccessSnapshot {
    /// Captures the active icon count and closed layout values.
    pub fn new(
        active_icons: usize,
        position: QuickAccessPosition,
        is_vertical: bool,
    ) -> Result<Self, NexusLinkSnapshotError> {
        let active_icons =
            i32::try_from(active_icons).map_err(|_| NexusLinkSnapshotError::IconCountOverflow)?;
        Ok(Self {
            active_icons,
            position,
            is_vertical,
        })
    }
}

/// Complete immutable input for one exact NexusLink ABI update.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NexusLinkSnapshot {
    render: RenderSnapshot,
    telemetry: DerivedTelemetry,
    quick_access: QuickAccessSnapshot,
}

impl NexusLinkSnapshot {
    /// Combines independently closed render, telemetry, and Quick Access snapshots.
    #[must_use]
    pub const fn new(
        render: RenderSnapshot,
        telemetry: DerivedTelemetry,
        quick_access: QuickAccessSnapshot,
    ) -> Self {
        Self {
            render,
            telemetry,
            quick_access,
        }
    }

    fn as_abi(self) -> NexusLinkData {
        NexusLinkData {
            width: self.render.width,
            height: self.render.height,
            scaling: self.render.scaling,
            is_moving: u8::from(self.telemetry.is_moving),
            is_camera_moving: u8::from(self.telemetry.is_camera_moving),
            is_gameplay: u8::from(self.telemetry.is_gameplay),
            font: self.render.fonts.regular as *mut c_void,
            font_big: self.render.fonts.large as *mut c_void,
            font_ui: self.render.fonts.ui as *mut c_void,
            quick_access_icons_count: self.quick_access.active_icons,
            quick_access_mode: self.quick_access.position as i32,
            quick_access_is_vertical: u8::from(self.quick_access.is_vertical),
        }
    }
}

/// Validation failures for closed NexusLink input snapshots.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NexusLinkSnapshotError {
    /// UI scaling must be finite and positive.
    #[error("the NexusLink UI scale must be finite and positive")]
    InvalidScaling,
    /// The active Quick Access icon count did not fit the legacy `int32_t` field.
    #[error("the Quick Access icon count exceeds the NexusLink ABI")]
    IconCountOverflow,
}

/// Failures while opening the exact public NexusLink resource.
#[derive(Debug, Error)]
pub enum NexusLinkOpenError {
    /// DataLink could not open or create the public resource.
    #[error(transparent)]
    DataLink(#[from] DataServiceError),
    /// The supplied lease was not a public named mapping.
    #[error("the NexusLink publisher requires a public mapping")]
    NotPublic,
    /// The supplied lease did not match the exact NexusLink ABI size.
    #[error("the NexusLink resource has size {actual}, expected {expected}")]
    SizeMismatch {
        /// Exact ABI size.
        expected: usize,
        /// Supplied resource size.
        actual: usize,
    },
}

/// Serialized publisher for the exact `NexusLinkData` C layout.
pub struct NexusLinkPublisher {
    resource: ResourceLease,
    writer: Mutex<()>,
}

impl NexusLinkPublisher {
    /// Opens the legacy public resource and retains its mapping lifetime.
    pub fn open(
        data_link: &DataLinkService,
        underlying_name: Option<&str>,
    ) -> Result<Self, NexusLinkOpenError> {
        let resource =
            data_link.share_public(DL_NEXUS_LINK, size_of::<NexusLinkData>(), underlying_name)?;
        Self::new(resource)
    }

    /// Creates a publisher from an already retained exact public resource.
    pub fn new(resource: ResourceLease) -> Result<Self, NexusLinkOpenError> {
        if resource.kind() != ResourceKind::Public {
            return Err(NexusLinkOpenError::NotPublic);
        }
        if resource.len() != size_of::<NexusLinkData>() {
            return Err(NexusLinkOpenError::SizeMismatch {
                expected: size_of::<NexusLinkData>(),
                actual: resource.len(),
            });
        }
        Ok(Self {
            resource,
            writer: Mutex::new(()),
        })
    }

    /// Publishes every ABI field from one immutable closed snapshot.
    ///
    /// Publication is serialized among Rust producers. Native consumers retain
    /// the legacy shared-memory synchronization contract.
    pub fn publish(&self, snapshot: NexusLinkSnapshot) {
        let _writer = mutex_lock(&self.writer);
        let abi = snapshot.as_abi();
        let target = self.resource.as_mut_ptr().cast::<NexusLinkData>();
        // SAFETY: construction proves that the retained writable mapping is
        // exactly one `NexusLinkData` long. Field-address writes are unaligned,
        // so injected byte-backed mappings are also valid. Every source field
        // has a valid ABI representation, including byte-backed booleans.
        unsafe { write_abi_fields(target, &abi) };
    }

    /// Returns the retained public DataLink lease.
    #[must_use]
    pub const fn resource(&self) -> &ResourceLease {
        &self.resource
    }
}

unsafe fn write_abi_fields(target: *mut NexusLinkData, source: &NexusLinkData) {
    let base = target.cast::<u8>();
    macro_rules! write_field {
        ($field:ident) => {{
            let destination = base.add(offset_of!(NexusLinkData, $field)).cast::<_>();
            ptr::write_unaligned(destination, source.$field);
        }};
    }

    // SAFETY: the caller guarantees a complete writable ABI object. Offsets
    // are obtained from the same `repr(C)` type and each write stays in-bounds.
    unsafe {
        write_field!(width);
        write_field!(height);
        write_field!(scaling);
        write_field!(is_moving);
        write_field!(is_camera_moving);
        write_field!(is_gameplay);
        write_field!(font);
        write_field!(font_big);
        write_field!(font_ui);
        write_field!(quick_access_icons_count);
        write_field!(quick_access_mode);
        write_field!(quick_access_is_vertical);
    }
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;
    use std::sync::Arc;

    use nexus_abi::NexusLinkData;
    use nexus_gw2::DerivedTelemetry;

    use crate::mapping::MappingBackend;
    use crate::test_support::MemoryMappingBackend;
    use crate::{DataLinkService, ResourceKind};

    use super::{
        FontSnapshot, NexusLinkPublisher, NexusLinkSnapshot, QuickAccessPosition,
        QuickAccessSnapshot, RenderSnapshot,
    };

    #[test]
    fn publishes_every_exact_abi_field_from_closed_snapshots() {
        let backend: Arc<dyn MappingBackend> = Arc::new(MemoryMappingBackend::default());
        let data_link = DataLinkService::with_process_id(backend, 99);
        let publisher = NexusLinkPublisher::open(&data_link, Some("Local\\NexusPublisherTest"))
            .expect("the injected public mapping should open");
        assert_eq!(publisher.resource().kind(), ResourceKind::Public);
        assert_eq!(publisher.resource().len(), size_of::<NexusLinkData>());

        let render = RenderSnapshot::new(
            2_560,
            1_440,
            1.11,
            FontSnapshot::from_addresses(0x10, 0x20, 0x30),
        )
        .expect("the render fixture is valid");
        let quick_access = QuickAccessSnapshot::new(12, QuickAccessPosition::Custom, true)
            .expect("the Quick Access fixture is valid");
        publisher.publish(NexusLinkSnapshot::new(
            render,
            DerivedTelemetry {
                is_moving: true,
                is_camera_moving: false,
                is_gameplay: true,
            },
            quick_access,
        ));

        // SAFETY: the publisher retains an exact-size resource and publication
        // initialized every field. Unaligned read supports the injected backend.
        let data = unsafe {
            publisher
                .resource()
                .as_mut_ptr()
                .cast::<NexusLinkData>()
                .read_unaligned()
        };
        assert_eq!(data.width, 2_560);
        assert_eq!(data.height, 1_440);
        assert_eq!(data.scaling, 1.11);
        assert_eq!(data.is_moving, 1);
        assert_eq!(data.is_camera_moving, 0);
        assert_eq!(data.is_gameplay, 1);
        assert_eq!(data.font as usize, 0x10);
        assert_eq!(data.font_big as usize, 0x20);
        assert_eq!(data.font_ui as usize, 0x30);
        assert_eq!(data.quick_access_icons_count, 12);
        assert_eq!(data.quick_access_mode, QuickAccessPosition::Custom as i32);
        assert_eq!(data.quick_access_is_vertical, 1);
    }

    #[test]
    fn snapshot_validation_is_closed() {
        assert!(RenderSnapshot::new(1, 1, f32::NAN, FontSnapshot::default()).is_err());
        assert!(
            QuickAccessSnapshot::new(
                usize::try_from(i32::MAX).expect("i32::MAX fits usize") + 1,
                QuickAccessPosition::Extend,
                false
            )
            .is_err()
        );
    }
}
