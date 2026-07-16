use core::{ffi::c_char, ops};

use crate::Version;

/// Opaque base object passed to an add-on's load callback.
#[repr(C)]
pub struct AddonApi {
    _private: [u8; 0],
}

/// Add-on entry point invoked after the requested API revision is assembled.
pub type AddonLoad = unsafe extern "C" fn(api: *mut AddonApi);

/// Add-on entry point invoked before its module is unloaded.
pub type AddonUnload = unsafe extern "C" fn();

/// Exported `GetAddonDef` function implemented by Nexus add-ons.
pub type GetAddonDefinitionV1 = unsafe extern "C" fn() -> *mut AddonDefinitionV1;

macro_rules! flags {
    (
        $(#[$meta:meta])*
        $visibility:vis struct $name:ident {
            $($(#[$flag_meta:meta])* const $flag:ident = $value:expr;)+
        }
    ) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        $visibility struct $name(u32);

        impl $name {
            /// No flags are set.
            pub const NONE: Self = Self(0);
            $($(#[$flag_meta])* pub const $flag: Self = Self($value);)+

            /// Returns the raw ABI bits.
            #[must_use]
            pub const fn bits(self) -> u32 {
                self.0
            }

            /// Creates flags without discarding bits added by a newer host.
            #[must_use]
            pub const fn from_bits_retain(bits: u32) -> Self {
                Self(bits)
            }

            /// Returns whether every bit in `other` is present.
            #[must_use]
            pub const fn contains(self, other: Self) -> bool {
                (self.0 & other.0) == other.0
            }
        }

        impl ops::BitOr for $name {
            type Output = Self;

            fn bitor(self, rhs: Self) -> Self::Output {
                Self(self.0 | rhs.0)
            }
        }

        impl ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl ops::BitAnd for $name {
            type Output = Self;

            fn bitand(self, rhs: Self) -> Self::Output {
                Self(self.0 & rhs.0)
            }
        }
    };
}

flags! {
    /// Interfaces detected in a native add-on module.
    pub struct AddonInterfaces {
        /// Native Nexus `GetAddonDef` interface.
        const NEXUS = 1 << 0;
        /// ArcDPS extension interface.
        const ARC_DPS = 1 << 1;
        /// GW2 Addon Loader interface.
        const ADDON_LOADER = 1 << 2;
        /// D3D11 proxy exports.
        const D3D11_PROXY = 1 << 3;
        /// DXGI proxy exports.
        const DXGI_PROXY = 1 << 4;
    }
}

flags! {
    /// Host-internal add-on state bits retained here for layout and migration parity.
    pub struct AddonRuntimeFlags {
        /// An asynchronous lifecycle action is running.
        const RUNNING_ACTION = 1 << 0;
        /// The host is destroying the add-on record.
        const DESTROYING = 1 << 1;
        /// The DLL file cannot be replaced while loaded.
        const FILE_LOCKED = 1 << 2;
        /// Runtime load/unload transitions are disabled.
        const STATE_LOCKED = 1 << 3;
        /// The exported definition is incomplete or unsupported.
        const MISSING_REQUIREMENTS = 1 << 4;
        /// The add-on has been marked for removal.
        const UNINSTALLED = 1 << 5;
        /// A newer add-on build is available.
        const UPDATE_AVAILABLE = 1 << 6;
    }
}

flags! {
    /// Behavior flags exported by an add-on definition.
    pub struct AddonDefinitionFlags {
        /// Disable automatically when the Guild Wars 2 build changes.
        const VOLATILE = 1 << 0;
        /// Prevent runtime unloading while still allowing shutdown cleanup.
        const DISABLE_HOT_LOADING = 1 << 1;
        /// Only load during initial host startup.
        const LAUNCH_ONLY = 1 << 2;
        /// The add-on can create and own an independent Dear ImGui context.
        const CAN_CREATE_IMGUI_CONTEXT = 1 << 3;
        /// Keep the add-on updated regardless of ordinary user update settings.
        const FORCE_UPDATE = 1 << 4;
    }
}

/// Update source selected by an add-on definition.
///
/// This is a transparent integer rather than a Rust enum so an unknown value
/// from a newer add-on never creates an invalid Rust discriminant.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpdateProvider(u32);

impl UpdateProvider {
    /// Automatic updates are unsupported.
    pub const NONE: Self = Self(0);
    /// Raidcore API.
    pub const RAIDCORE: Self = Self(1);
    /// GitHub Releases.
    pub const GITHUB: Self = Self(2);
    /// Direct file URL.
    pub const DIRECT: Self = Self(3);
    /// Add-on-managed update check.
    pub const SELF_MANAGED: Self = Self(4);

    /// Returns the raw ABI value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// Preserves an update-provider value supplied by a newer add-on.
    #[must_use]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }
}

/// Raw definition exported by every Nexus add-on using API revisions 1–6.
///
/// String pointers and callbacks are borrowed from the loaded add-on module.
/// A host must validate and copy them before the module can be unloaded.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AddonDefinitionV1 {
    /// Stable, non-zero add-on identifier.
    pub signature: u32,
    /// Requested Nexus API revision.
    pub api_version: u32,
    /// NUL-terminated display name.
    pub name: *const c_char,
    /// Add-on version.
    pub version: Version,
    /// NUL-terminated author name.
    pub author: *const c_char,
    /// NUL-terminated short description.
    pub description: *const c_char,
    /// Required load callback.
    pub load: Option<AddonLoad>,
    /// Unload callback, optional only when hot loading is disabled.
    pub unload: Option<AddonUnload>,
    /// Add-on behavior flags.
    pub flags: AddonDefinitionFlags,
    /// Update source.
    pub provider: UpdateProvider,
    /// Optional NUL-terminated update URL.
    pub update_link: *const c_char,
}

impl AddonDefinitionV1 {
    /// Mirrors the legacy definition's structural minimum-requirements check.
    #[must_use]
    pub const fn has_minimum_requirements(&self) -> bool {
        let supports_unload = self.unload.is_some()
            || self
                .flags
                .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING);

        self.signature != 0
            && self.api_version != 0
            && !self.name.is_null()
            && !self.author.is_null()
            && !self.description.is_null()
            && self.load.is_some()
            && supports_unload
    }
}

#[cfg(test)]
mod tests {
    use core::{
        ffi::c_char,
        mem::{align_of, offset_of, size_of},
        ptr,
    };

    use super::{
        AddonApi, AddonDefinitionFlags, AddonDefinitionV1, AddonInterfaces, UpdateProvider,
    };
    use crate::Version;

    unsafe extern "C" fn load_stub(_api: *mut AddonApi) {}
    unsafe extern "C" fn unload_stub() {}

    #[test]
    fn addon_definition_layout_matches_msvc_x64() {
        assert_eq!(size_of::<AddonDefinitionV1>(), 72);
        assert_eq!(align_of::<AddonDefinitionV1>(), 8);
        assert_eq!(offset_of!(AddonDefinitionV1, signature), 0);
        assert_eq!(offset_of!(AddonDefinitionV1, api_version), 4);
        assert_eq!(offset_of!(AddonDefinitionV1, name), 8);
        assert_eq!(offset_of!(AddonDefinitionV1, version), 16);
        assert_eq!(offset_of!(AddonDefinitionV1, author), 24);
        assert_eq!(offset_of!(AddonDefinitionV1, description), 32);
        assert_eq!(offset_of!(AddonDefinitionV1, load), 40);
        assert_eq!(offset_of!(AddonDefinitionV1, unload), 48);
        assert_eq!(offset_of!(AddonDefinitionV1, flags), 56);
        assert_eq!(offset_of!(AddonDefinitionV1, provider), 60);
        assert_eq!(offset_of!(AddonDefinitionV1, update_link), 64);
    }

    #[test]
    fn legacy_flag_values_are_stable() {
        assert_eq!(AddonInterfaces::DXGI_PROXY.bits(), 1 << 4);
        assert_eq!(AddonDefinitionFlags::FORCE_UPDATE.bits(), 1 << 4);
        assert_eq!(UpdateProvider::SELF_MANAGED.value(), 4);
    }

    #[test]
    fn minimum_requirements_accept_unload_or_locked_lifetime() {
        static TEXT: &[u8] = b"x\0";
        let text = TEXT.as_ptr().cast::<c_char>();
        let mut definition = AddonDefinitionV1 {
            signature: 1,
            api_version: 6,
            name: text,
            version: Version::new(1, 2, 3, 0),
            author: text,
            description: text,
            load: Some(load_stub),
            unload: Some(unload_stub),
            flags: AddonDefinitionFlags::NONE,
            provider: UpdateProvider::NONE,
            update_link: ptr::null(),
        };

        assert!(definition.has_minimum_requirements());
        definition.unload = None;
        assert!(!definition.has_minimum_requirements());
        definition.flags = AddonDefinitionFlags::DISABLE_HOT_LOADING;
        assert!(definition.has_minimum_requirements());
    }
}
