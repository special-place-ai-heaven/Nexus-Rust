//! Owner-scoped font registry and UI-thread atlas rebuild coordinator.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;

use nexus_imgui_compat::sys;
use thiserror::Error;

use crate::OwnerId;

/// Closed failures produced while loading font bytes.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FontAssetError {
    /// The requested asset does not exist.
    #[error("font asset was not found")]
    NotFound,
    /// The requested asset could not be read.
    #[error("font asset could not be read")]
    Unreadable,
    /// This loader does not implement the requested source kind.
    #[error("font asset source is unsupported")]
    Unsupported,
}

/// Opaque Windows-resource identity passed to an injected loader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceFont {
    /// Process-local module token; zero is invalid.
    pub module: usize,
    /// Numeric `RT_FONT` resource identifier.
    pub resource_id: u32,
}

/// Injected file/resource byte loader.
pub trait FontAssetLoader {
    /// Loads a font file without transferring ownership of the path.
    fn load_file(&mut self, path: &Path) -> Result<Vec<u8>, FontAssetError>;

    /// Copies bytes from an embedded module resource.
    fn load_resource(&mut self, resource: ResourceFont) -> Result<Vec<u8>, FontAssetError>;
}

/// Standard file loader; module resources remain a platform responsibility.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileFontAssetLoader;

impl FontAssetLoader for FileFontAssetLoader {
    fn load_file(&mut self, path: &Path) -> Result<Vec<u8>, FontAssetError> {
        match fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => Ok(bytes),
            Ok(_) => Err(FontAssetError::Unreadable),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(FontAssetError::NotFound)
            }
            Err(_) => Err(FontAssetError::Unreadable),
        }
    }

    fn load_resource(&mut self, _resource: ResourceFont) -> Result<Vec<u8>, FontAssetError> {
        Err(FontAssetError::Unsupported)
    }
}

/// Deep-owned subset of Dear ImGui 1.80's `ImFontConfig`.
///
/// Atlas-owned data pointers and destination-font pointers are deliberately not
/// accepted. Glyph ranges are copied so addon unload cannot leave dangling
/// pointers in a later atlas rebuild.
#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    /// Font index within a TTC/OTC collection.
    pub font_no: i32,
    /// Horizontal rasterizer oversampling.
    pub oversample_h: i32,
    /// Vertical rasterizer oversampling.
    pub oversample_v: i32,
    /// Whether glyphs snap to pixel boundaries.
    pub pixel_snap_h: bool,
    /// Additional horizontal and vertical glyph spacing.
    pub glyph_extra_spacing: [f32; 2],
    /// Glyph offset applied during rasterization.
    pub glyph_offset: [f32; 2],
    /// Owned inclusive start/end pairs followed by a zero terminator.
    pub glyph_ranges: Vec<u16>,
    /// Minimum glyph advance.
    pub glyph_min_advance_x: f32,
    /// Maximum glyph advance.
    pub glyph_max_advance_x: f32,
    /// Merge this input into the preceding atlas font.
    pub merge_mode: bool,
    /// Rasterizer-specific flags.
    pub rasterizer_flags: u32,
    /// Rasterizer brightness multiplier.
    pub rasterizer_multiply: f32,
    /// Explicit ellipsis character, or `u16::MAX` for automatic selection.
    pub ellipsis_char: u16,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            font_no: 0,
            oversample_h: 3,
            oversample_v: 1,
            pixel_snap_h: false,
            glyph_extra_spacing: [0.0; 2],
            glyph_offset: [0.0; 2],
            glyph_ranges: Vec::new(),
            glyph_min_advance_x: 0.0,
            glyph_max_advance_x: f32::MAX,
            merge_mode: false,
            rasterizer_flags: 0,
            rasterizer_multiply: 1.0,
            ellipsis_char: u16::MAX,
        }
    }
}

/// Pointer to a font owned by the current Dear ImGui 1.80 atlas generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct FontHandle(NonNull<sys::ImFont>);

impl FontHandle {
    /// Wraps a font pointer returned by a successful Dear ImGui atlas add.
    ///
    /// # Safety
    ///
    /// `pointer` must refer to a live `ImFont` in the backend's current atlas.
    /// The manager never dereferences it, but native callbacks may do so until
    /// they receive the mandatory `None` notification before the next rebuild.
    pub unsafe fn from_ptr(pointer: *mut sys::ImFont) -> Option<Self> {
        NonNull::new(pointer).map(Self)
    }

    /// Returns the native pointer passed to addon callbacks.
    #[must_use]
    pub fn as_ptr(self) -> *mut sys::ImFont {
        self.0.as_ptr()
    }
}

/// Which legacy glyph-range policy applies to an atlas input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlyphCoverage {
    /// Default and Latin Extended ranges plus localized strings.
    Localized,
    /// The host default additionally includes full Chinese and Cyrillic ranges.
    HostDefault,
}

/// Immutable font input borrowed for one backend rebuild.
#[derive(Clone, Copy, Debug)]
pub struct FontBuildRequest<'font> {
    /// Stable native identifier.
    pub identifier: &'font CStr,
    /// Owned font file bytes.
    pub data: &'font [u8],
    /// Requested rasterized size.
    pub size: f32,
    /// Deep-owned configuration.
    pub config: &'font FontConfig,
    /// Legacy Nexus glyph policy.
    pub coverage: GlyphCoverage,
}

/// Redaction-safe failures from a platform-specific atlas backend.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FontBackendError {
    /// Dear ImGui rejected atlas input.
    #[error("font atlas rejected an input")]
    RejectedInput,
    /// Atlas rasterization failed.
    #[error("font atlas build failed")]
    BuildFailed,
    /// Renderer font-texture recreation failed.
    #[error("font texture recreation failed")]
    TextureFailed,
}

/// Thread-local Dear ImGui 1.80 atlas backend.
///
/// The trait intentionally has no `Send` or `Sync` bound. [`FontManager`] also
/// carries an explicit `Rc` marker, preventing accidental cross-thread moves.
pub trait FontAtlasBackend {
    /// Clears and rebuilds the complete atlas, returning one handle per input.
    fn rebuild(
        &mut self,
        fonts: &[FontBuildRequest<'_>],
        localized_texts: &[&CStr],
    ) -> Result<Vec<Option<FontHandle>>, FontBackendError>;
}

/// Callback invoked whenever a font pointer becomes invalid or available.
pub type FontCallback = Box<dyn FnMut(&CStr, Option<FontHandle>) + 'static>;

/// Stable token for releasing one callback subscription.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// Returns the opaque numeric token.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Result of registering a font source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontRegistration {
    /// Callback token, if a callback was supplied.
    pub subscription: Option<SubscriptionId>,
    /// Whether this registration created the underlying font.
    pub created: bool,
}

/// Shared inputs for file- and resource-backed font registration.
pub struct FontRegistrationRequest<'identifier> {
    /// Addon-generation owner used during unload cleanup.
    pub owner: OwnerId,
    /// Stable font identifier from the addon API.
    pub identifier: &'identifier str,
    /// Requested rasterized size.
    pub size: f32,
    /// Deep-owned Dear ImGui configuration.
    pub config: FontConfig,
    /// Optional update callback.
    pub callback: Option<FontCallback>,
}

/// Borrowed input for one owner-scoped memory replacement transaction.
///
/// The manager validates every input before mutation, then copies the bytes and
/// deep-owned configuration during the atomic commit.
#[derive(Clone, Copy)]
pub struct FontMemoryReplacement<'font> {
    /// Stable font identifier.
    pub identifier: &'font str,
    /// Requested rasterized size.
    pub size: f32,
    /// Borrowed font-file bytes copied by the manager.
    pub data: &'font [u8],
    /// Borrowed configuration cloned by the manager.
    pub config: &'font FontConfig,
}

impl fmt::Debug for FontMemoryReplacement<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FontMemoryReplacement")
            .field("identifier", &self.identifier)
            .field("size", &self.size)
            .field("byte_len", &self.data.len())
            .field("config", &self.config)
            .finish()
    }
}

/// Closed outcome of one successful owner-scoped replacement transaction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FontOwnerReplaceReport {
    /// Number of desired inputs in the transaction.
    pub requested: usize,
    /// Number of new registry entries created.
    pub created: usize,
    /// Number of existing owner-claimed entries updated in place.
    pub updated: usize,
    /// Number of this owner's claims removed from omitted entries.
    pub removed_claims: usize,
    /// Number of omitted entries removed after becoming unreferenced.
    pub removed_entries: usize,
    /// Number of omitted entries retained for subscribers or foreign claims.
    pub retained_omitted: usize,
}

/// Closed, redaction-safe replacement preflight failures.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum FontOwnerReplaceError {
    /// A zero-based request failed ordinary font validation.
    #[error("font replacement request {request_index} is invalid")]
    InvalidRequest {
        /// Zero-based position in the submitted request slice.
        request_index: usize,
        /// Closed validation failure returned by the font service.
        #[source]
        source: FontError,
    },
    /// A zero-based request repeats an earlier identifier.
    #[error("font replacement request {request_index} duplicates an identifier")]
    DuplicateIdentifier {
        /// Zero-based position of the duplicate request.
        request_index: usize,
    },
    /// An existing identifier is not claimed by the replacing owner.
    #[error("font replacement request {request_index} conflicts with another owner")]
    OwnerConflict {
        /// Zero-based position of the conflicting request.
        request_index: usize,
    },
}

/// Result of the legacy immediate `Get` callback behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontGetResult {
    /// Stored subscription for an existing font; absent for a miss.
    pub subscription: Option<SubscriptionId>,
    /// Whether the immediate callback panicked and was contained.
    pub callback_panicked: bool,
}

/// Result of advancing font-atlas state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FontAdvance {
    /// Whether a rebuild occurred.
    pub rebuilt: bool,
    /// Addon callback panics contained during invalidation and publication.
    pub callback_panics: usize,
}

/// Redaction-safe font registry failures.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum FontError {
    /// Identifier is empty or cannot cross the native string ABI.
    #[error("font identifier is invalid")]
    InvalidIdentifier,
    /// Size is non-finite or smaller than one pixel.
    #[error("font size is invalid")]
    InvalidSize,
    /// Font data is empty.
    #[error("font data is empty")]
    EmptyData,
    /// Font configuration is inconsistent.
    #[error("font configuration is invalid")]
    InvalidConfig,
    /// The requested font is not registered.
    #[error("font is not registered")]
    UnknownFont,
    /// An injected asset loader failed.
    #[error(transparent)]
    Asset(#[from] FontAssetError),
    /// The atlas backend failed.
    #[error(transparent)]
    Backend(#[from] FontBackendError),
    /// Backend result count or a returned font pointer was incomplete.
    #[error("font atlas returned an incomplete generation")]
    IncompleteBuild,
}

struct Subscriber {
    id: SubscriptionId,
    owner: OwnerId,
    callback: FontCallback,
}

struct FontEntry {
    identifier: CString,
    size: f32,
    data: Vec<u8>,
    config: FontConfig,
    handle: Option<FontHandle>,
    owner_claims: BTreeSet<OwnerId>,
    subscribers: Vec<Subscriber>,
}

struct PreparedFontReplacement {
    identifier: CString,
    size: f32,
    data: Vec<u8>,
    config: FontConfig,
}

impl FontEntry {
    fn is_unreferenced(&self) -> bool {
        self.owner_claims.is_empty() && self.subscribers.is_empty()
    }
}

/// Complete font registry and atlas coordinator.
pub struct FontManager<B> {
    backend: B,
    registry: Vec<FontEntry>,
    atlas_built: bool,
    next_subscription: u64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<B: FontAtlasBackend> FontManager<B> {
    /// Creates a dirty registry that will build on its first advance.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            registry: Vec::new(),
            atlas_built: false,
            next_subscription: 0,
            _thread_bound: PhantomData,
        }
    }

    /// Registers copied in-memory bytes.
    pub fn register_memory(
        &mut self,
        owner: OwnerId,
        identifier: &str,
        size: f32,
        data: &[u8],
        config: FontConfig,
        callback: Option<FontCallback>,
    ) -> Result<FontRegistration, FontError> {
        validate_registration(identifier, size, data, &config)?;
        self.register_owned(owner, identifier, size, data.to_vec(), config, callback)
    }

    /// Registers bytes loaded from a file through an injected source.
    pub fn register_file(
        &mut self,
        loader: &mut impl FontAssetLoader,
        path: &Path,
        request: FontRegistrationRequest<'_>,
    ) -> Result<FontRegistration, FontError> {
        let data = loader.load_file(path)?;
        self.register_memory(
            request.owner,
            request.identifier,
            request.size,
            &data,
            request.config,
            request.callback,
        )
    }

    /// Registers bytes loaded from a module resource through an injected source.
    pub fn register_resource(
        &mut self,
        loader: &mut impl FontAssetLoader,
        resource: ResourceFont,
        request: FontRegistrationRequest<'_>,
    ) -> Result<FontRegistration, FontError> {
        if resource.module == 0 || resource.resource_id == 0 {
            return Err(FontAssetError::NotFound.into());
        }
        let data = loader.load_resource(resource)?;
        self.register_memory(
            request.owner,
            request.identifier,
            request.size,
            &data,
            request.config,
            request.callback,
        )
    }

    /// Atomically replaces every memory-backed font claimed by one owner.
    ///
    /// Validation, duplicate detection, owner-conflict checks, and all borrowed
    /// input copies complete before the registry is touched. Existing requested
    /// entries retain every subscriber and foreign owner claim. Omitted entries
    /// lose only `owner`'s claim and remain registered while any other claim or
    /// subscriber exists.
    ///
    /// Requested entries form one contiguous block in the exact submitted
    /// order at the replacing owner's previous first position, or at the end
    /// for a new owner. This preserves Dear ImGui merge adjacency.
    pub fn replace_owner_memory(
        &mut self,
        owner: OwnerId,
        replacements: &[FontMemoryReplacement<'_>],
    ) -> Result<FontOwnerReplaceReport, FontOwnerReplaceError> {
        let mut identifiers = BTreeSet::new();
        for (request_index, replacement) in replacements.iter().enumerate() {
            validate_registration(
                replacement.identifier,
                replacement.size,
                replacement.data,
                replacement.config,
            )
            .map_err(|source| FontOwnerReplaceError::InvalidRequest {
                request_index,
                source,
            })?;
            if !identifiers.insert(replacement.identifier) {
                return Err(FontOwnerReplaceError::DuplicateIdentifier { request_index });
            }
        }

        for (request_index, replacement) in replacements.iter().enumerate() {
            if let Some(index) = self.find(replacement.identifier)
                && !self.registry[index].owner_claims.contains(&owner)
            {
                return Err(FontOwnerReplaceError::OwnerConflict { request_index });
            }
        }

        let prepared = replacements
            .iter()
            .enumerate()
            .map(|(request_index, replacement)| {
                let identifier = CString::new(replacement.identifier).map_err(|_| {
                    FontOwnerReplaceError::InvalidRequest {
                        request_index,
                        source: FontError::InvalidIdentifier,
                    }
                })?;
                Ok(PreparedFontReplacement {
                    identifier,
                    size: replacement.size,
                    data: replacement.data.to_vec(),
                    config: replacement.config.clone(),
                })
            })
            .collect::<Result<Vec<_>, FontOwnerReplaceError>>()?;

        let owner_anchor = self
            .registry
            .iter()
            .position(|entry| entry.owner_claims.contains(&owner))
            .unwrap_or(self.registry.len());
        let previous = std::mem::take(&mut self.registry);
        let mut desired_slots = (0..prepared.len()).map(|_| None).collect::<Vec<_>>();
        let mut remaining = Vec::with_capacity(previous.len());
        let mut insertion_index = 0;
        let mut report = FontOwnerReplaceReport {
            requested: prepared.len(),
            ..FontOwnerReplaceReport::default()
        };

        for (old_index, mut entry) in previous.into_iter().enumerate() {
            if let Some(request_index) = prepared.iter().position(|replacement| {
                entry.identifier.as_bytes() == replacement.identifier.as_bytes()
            }) {
                desired_slots[request_index] = Some(entry);
                continue;
            }

            let removed_claim = entry.owner_claims.remove(&owner);
            if removed_claim {
                report.removed_claims += 1;
                if entry.is_unreferenced() {
                    report.removed_entries += 1;
                    continue;
                }
                report.retained_omitted += 1;
            }
            if old_index < owner_anchor {
                insertion_index += 1;
            }
            remaining.push(entry);
        }

        let mut desired = Vec::with_capacity(prepared.len());
        for (replacement, existing) in prepared.into_iter().zip(desired_slots) {
            if let Some(mut entry) = existing {
                entry.identifier = replacement.identifier;
                entry.size = replacement.size;
                entry.data = replacement.data;
                entry.config = replacement.config;
                desired.push(entry);
                report.updated += 1;
            } else {
                let mut owner_claims = BTreeSet::new();
                owner_claims.insert(owner);
                desired.push(FontEntry {
                    identifier: replacement.identifier,
                    size: replacement.size,
                    data: replacement.data,
                    config: replacement.config,
                    handle: None,
                    owner_claims,
                    subscribers: Vec::new(),
                });
                report.created += 1;
            }
        }
        drop(remaining.splice(insertion_index..insertion_index, desired));
        self.registry = remaining;
        self.atlas_built = false;
        Ok(report)
    }

    /// Implements legacy `Get`: callback immediately receives the current font
    /// or `None`, and is retained only when the identifier exists.
    pub fn get(
        &mut self,
        owner: OwnerId,
        identifier: &str,
        mut callback: FontCallback,
    ) -> Result<FontGetResult, FontError> {
        validate_identifier(identifier)?;
        let Some(index) = self.find(identifier) else {
            let identifier = CString::new(identifier).map_err(|_| FontError::InvalidIdentifier)?;
            let callback_panicked = invoke_callback(&mut callback, &identifier, None);
            return Ok(FontGetResult {
                subscription: None,
                callback_panicked,
            });
        };
        let subscription = self.allocate_subscription();
        let entry = &mut self.registry[index];
        let callback_panicked = invoke_callback(&mut callback, &entry.identifier, entry.handle);
        entry.subscribers.push(Subscriber {
            id: subscription,
            owner,
            callback,
        });
        Ok(FontGetResult {
            subscription: Some(subscription),
            callback_panicked,
        })
    }

    /// Releases one callback. An otherwise unowned font is removed and the
    /// atlas is invalidated.
    pub fn release(&mut self, identifier: &str, subscription: SubscriptionId) -> bool {
        let Some(index) = self.find(identifier) else {
            return false;
        };
        let entry = &mut self.registry[index];
        let before = entry.subscribers.len();
        entry
            .subscribers
            .retain(|subscriber| subscriber.id != subscription);
        let removed = before != entry.subscribers.len();
        if removed && entry.is_unreferenced() {
            self.registry.remove(index);
            self.atlas_built = false;
        }
        removed
    }

    /// Removes only the callback subscribers for one exact addon generation.
    ///
    /// This is the pre-drain half of owner cleanup. Font entries stay
    /// registered and the atlas is deliberately left valid, because resources
    /// must remain available to in-flight callbacks until
    /// [`Self::cleanup_owner_resources`] runs after the callback gate drains.
    pub fn cleanup_owner_callbacks(&mut self, owner: OwnerId) -> usize {
        let mut removed = 0;
        for entry in &mut self.registry {
            let before = entry.subscribers.len();
            entry
                .subscribers
                .retain(|subscriber| subscriber.owner != owner);
            removed += before - entry.subscribers.len();
        }
        removed
    }

    /// Removes exact-generation owner claims and sweeps unreferenced entries.
    ///
    /// This is the post-drain half of owner cleanup. The sweep also collects
    /// entries that only became unreferenced during
    /// [`Self::cleanup_owner_callbacks`], and the atlas is invalidated only
    /// when the registry contents actually changed.
    pub fn cleanup_owner_resources(&mut self, owner: OwnerId) -> usize {
        let mut removed = 0;
        for entry in &mut self.registry {
            if entry.owner_claims.remove(&owner) {
                removed += 1;
            }
        }
        let before = self.registry.len();
        self.registry.retain(|entry| !entry.is_unreferenced());
        if before != self.registry.len() {
            self.atlas_built = false;
        }
        removed
    }

    /// Removes registrations and callbacks belonging to an addon generation.
    ///
    /// Retained as the combined legacy barrier: it runs the callback phase
    /// then the resource phase and returns their summed removal count.
    pub fn cleanup_owner(&mut self, owner: OwnerId) -> usize {
        self.cleanup_owner_callbacks(owner) + self.cleanup_owner_resources(owner)
    }

    /// Updates a registered font size and schedules an atlas rebuild.
    pub fn resize(&mut self, identifier: &str, size: f32) -> Result<bool, FontError> {
        validate_size(size)?;
        let index = self.find(identifier).ok_or(FontError::UnknownFont)?;
        let font = &mut self.registry[index];
        if font.size == size {
            return Ok(false);
        }
        font.size = size;
        self.atlas_built = false;
        Ok(true)
    }

    /// Forces a complete rebuild on the next [`Self::advance`].
    pub fn reload(&mut self) {
        self.atlas_built = false;
    }

    /// Invalidates old callbacks, rebuilds once, then publishes new pointers.
    pub fn advance(&mut self, localized_texts: &[&CStr]) -> Result<FontAdvance, FontError> {
        if self.atlas_built {
            return Ok(FontAdvance::default());
        }

        let mut callback_panics = self.notify_callbacks(true);
        for entry in &mut self.registry {
            entry.handle = None;
        }

        let requests = self
            .registry
            .iter()
            .map(|font| FontBuildRequest {
                identifier: &font.identifier,
                data: &font.data,
                size: font.size,
                config: &font.config,
                coverage: if font.identifier.as_bytes() == b"FONT_DEFAULT" {
                    GlyphCoverage::HostDefault
                } else {
                    GlyphCoverage::Localized
                },
            })
            .collect::<Vec<_>>();
        let handles = self.backend.rebuild(&requests, localized_texts)?;
        if handles.len() != self.registry.len() || handles.iter().any(Option::is_none) {
            return Err(FontError::IncompleteBuild);
        }
        for (entry, handle) in self.registry.iter_mut().zip(handles) {
            entry.handle = handle;
        }
        self.atlas_built = true;
        callback_panics += self.notify_callbacks(false);
        Ok(FontAdvance {
            rebuilt: true,
            callback_panics,
        })
    }

    /// Returns a current atlas pointer without adding a subscription.
    #[must_use]
    pub fn handle(&self, identifier: &str) -> Option<FontHandle> {
        self.find(identifier)
            .and_then(|index| self.registry[index].handle)
    }

    /// Number of registered fonts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.registry.len()
    }

    /// Whether no fonts are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// Returns the injected backend after orderly manager shutdown.
    #[must_use]
    pub fn into_backend(self) -> B {
        self.backend
    }

    fn register_owned(
        &mut self,
        owner: OwnerId,
        identifier: &str,
        size: f32,
        data: Vec<u8>,
        config: FontConfig,
        callback: Option<FontCallback>,
    ) -> Result<FontRegistration, FontError> {
        if let Some(index) = self.find(identifier) {
            let subscription = callback.map(|callback| {
                let id = self.allocate_subscription();
                self.registry[index].subscribers.push(Subscriber {
                    id,
                    owner,
                    callback,
                });
                id
            });
            if subscription.is_none() {
                self.registry[index].owner_claims.insert(owner);
            }
            return Ok(FontRegistration {
                subscription,
                created: false,
            });
        }

        let identifier = CString::new(identifier).map_err(|_| FontError::InvalidIdentifier)?;
        let subscription = callback.map(|callback| {
            let id = self.allocate_subscription();
            (id, callback)
        });
        let mut owner_claims = BTreeSet::new();
        let mut subscribers = Vec::new();
        let subscription_id = if let Some((id, callback)) = subscription {
            subscribers.push(Subscriber {
                id,
                owner,
                callback,
            });
            Some(id)
        } else {
            owner_claims.insert(owner);
            None
        };
        self.registry.push(FontEntry {
            identifier,
            size,
            data,
            config,
            handle: None,
            owner_claims,
            subscribers,
        });
        self.atlas_built = false;
        Ok(FontRegistration {
            subscription: subscription_id,
            created: true,
        })
    }

    fn find(&self, identifier: &str) -> Option<usize> {
        self.registry
            .iter()
            .position(|font| font.identifier.as_bytes() == identifier.as_bytes())
    }

    fn allocate_subscription(&mut self) -> SubscriptionId {
        self.next_subscription = self.next_subscription.saturating_add(1);
        SubscriptionId(self.next_subscription)
    }

    fn notify_callbacks(&mut self, notify_null: bool) -> usize {
        let mut panics = 0;
        for font in &mut self.registry {
            let handle = (!notify_null).then_some(font.handle).flatten();
            for subscriber in &mut font.subscribers {
                panics += usize::from(invoke_callback(
                    &mut subscriber.callback,
                    &font.identifier,
                    handle,
                ));
            }
        }
        panics
    }
}

fn invoke_callback(
    callback: &mut FontCallback,
    identifier: &CStr,
    font: Option<FontHandle>,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| callback(identifier, font))).is_err()
}

fn validate_registration(
    identifier: &str,
    size: f32,
    data: &[u8],
    config: &FontConfig,
) -> Result<(), FontError> {
    validate_identifier(identifier)?;
    validate_size(size)?;
    if data.is_empty() {
        return Err(FontError::EmptyData);
    }
    validate_config(config)
}

fn validate_identifier(identifier: &str) -> Result<(), FontError> {
    if identifier.is_empty() || identifier.as_bytes().contains(&0) {
        Err(FontError::InvalidIdentifier)
    } else {
        Ok(())
    }
}

fn validate_size(size: f32) -> Result<(), FontError> {
    if size.is_finite() && size >= 1.0 {
        Ok(())
    } else {
        Err(FontError::InvalidSize)
    }
}

fn validate_config(config: &FontConfig) -> Result<(), FontError> {
    if config.oversample_h < 1
        || config.oversample_v < 1
        || !config.glyph_min_advance_x.is_finite()
        || !config.glyph_max_advance_x.is_finite()
        || !config.rasterizer_multiply.is_finite()
        || config.rasterizer_multiply <= 0.0
        || config
            .glyph_extra_spacing
            .iter()
            .chain(config.glyph_offset.iter())
            .any(|value| !value.is_finite())
    {
        return Err(FontError::InvalidConfig);
    }
    if !config.glyph_ranges.is_empty() {
        if config.glyph_ranges.last() != Some(&0)
            || !(config.glyph_ranges.len() - 1).is_multiple_of(2)
        {
            return Err(FontError::InvalidConfig);
        }
        if config.glyph_ranges[..config.glyph_ranges.len() - 1]
            .chunks_exact(2)
            .any(|range| range[0] == 0 || range[0] > range[1])
        {
            return Err(FontError::InvalidConfig);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::CStr;
    use std::ptr::NonNull;
    use std::rc::Rc;

    use nexus_imgui_compat::sys;

    use super::{
        FontAdvance, FontAtlasBackend, FontBackendError, FontBuildRequest, FontConfig, FontHandle,
        FontManager, FontMemoryReplacement, FontOwnerReplaceError, FontOwnerReplaceReport,
        GlyphCoverage,
    };
    use crate::OwnerId;

    #[derive(Clone, Debug, PartialEq)]
    struct CapturedFont {
        identifier: Vec<u8>,
        data: Vec<u8>,
        size: f32,
        merge_mode: bool,
        coverage: GlyphCoverage,
    }

    #[derive(Default)]
    struct FakeBackend {
        generations: usize,
        coverages: Vec<GlyphCoverage>,
        fonts: Vec<CapturedFont>,
        localized_text_count: usize,
    }

    impl FontAtlasBackend for FakeBackend {
        fn rebuild(
            &mut self,
            fonts: &[FontBuildRequest<'_>],
            localized_texts: &[&CStr],
        ) -> Result<Vec<Option<FontHandle>>, FontBackendError> {
            self.generations += 1;
            self.coverages = fonts.iter().map(|font| font.coverage).collect();
            self.fonts = fonts
                .iter()
                .map(|font| CapturedFont {
                    identifier: font.identifier.to_bytes().to_vec(),
                    data: font.data.to_vec(),
                    size: font.size,
                    merge_mode: font.config.merge_mode,
                    coverage: font.coverage,
                })
                .collect();
            self.localized_text_count = localized_texts.len();
            Ok(fonts
                .iter()
                .map(|_| {
                    let pointer = NonNull::<sys::ImFont>::dangling().as_ptr();
                    // SAFETY: this fake handle is never dereferenced; it is used
                    // only to test publication and invalidation ordering.
                    unsafe { FontHandle::from_ptr(pointer) }
                })
                .collect())
        }
    }

    #[test]
    fn rebuild_invalidates_then_publishes_and_contains_callback_panics() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let callback_events = Rc::clone(&events);
        let mut manager = FontManager::new(FakeBackend::default());
        let callback = Box::new(move |_identifier: &CStr, font: Option<FontHandle>| {
            callback_events.borrow_mut().push(font.is_some());
            if font.is_none() {
                panic!("contained addon callback panic");
            }
        });
        let registration = manager
            .register_memory(
                OwnerId::new(1, 1),
                "FONT_DEFAULT",
                15.0,
                b"font bytes",
                FontConfig::default(),
                Some(callback),
            )
            .unwrap_or_else(|error| panic!("registration failed: {error}"));
        assert!(registration.created);
        let advance = manager
            .advance(&[])
            .unwrap_or_else(|error| panic!("advance failed: {error}"));
        assert_eq!(
            advance,
            FontAdvance {
                rebuilt: true,
                callback_panics: 1
            }
        );
        assert_eq!(&*events.borrow(), &[false, true]);
        assert!(manager.handle("FONT_DEFAULT").is_some());
        let backend = manager.into_backend();
        assert_eq!(backend.coverages, vec![GlyphCoverage::HostDefault]);
    }

    #[test]
    fn release_and_owner_cleanup_drop_unreferenced_fonts() {
        let mut manager = FontManager::new(FakeBackend::default());
        let registration = manager
            .register_memory(
                OwnerId::new(4, 1),
                "addon",
                16.0,
                b"font",
                FontConfig::default(),
                Some(Box::new(|_, _| {})),
            )
            .unwrap_or_else(|error| panic!("registration failed: {error}"));
        let subscription = registration
            .subscription
            .unwrap_or_else(|| panic!("callback registration had no subscription"));
        assert!(manager.release("addon", subscription));
        assert!(manager.is_empty());

        assert!(
            manager
                .register_memory(
                    OwnerId::new(5, 1),
                    "owned-without-callback",
                    16.0,
                    b"font",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert_eq!(manager.cleanup_owner(OwnerId::new(5, 1)), 1);
        assert!(manager.is_empty());
    }

    #[test]
    fn callback_cleanup_keeps_resources_and_the_atlas_until_the_resource_phase() {
        let owner = OwnerId::new(9, 1);
        let mut manager = FontManager::new(FakeBackend::default());
        manager
            .register_memory(
                owner,
                "phased",
                16.0,
                b"font",
                FontConfig::default(),
                Some(Box::new(|_, _| {})),
            )
            .unwrap_or_else(|error| panic!("registration failed: {error}"));
        manager.atlas_built = true;

        assert_eq!(manager.cleanup_owner_callbacks(owner), 1);
        assert_eq!(
            manager.len(),
            1,
            "font resources must outlive the callback phase so draining callbacks stay valid"
        );
        assert!(
            manager.atlas_built,
            "the callback phase must not invalidate the atlas"
        );

        assert_eq!(
            manager.cleanup_owner_callbacks(owner),
            0,
            "callback cleanup must be idempotent"
        );
        assert!(manager.atlas_built);

        assert_eq!(
            manager.cleanup_owner_resources(owner),
            0,
            "a callback-only registration holds no owner claim to remove"
        );
        assert!(
            manager.is_empty(),
            "the resource phase must sweep entries that the callback phase left unreferenced"
        );
        assert!(
            !manager.atlas_built,
            "sweeping a real entry must invalidate the atlas"
        );

        manager.atlas_built = true;
        assert_eq!(
            manager.cleanup_owner_resources(owner),
            0,
            "resource cleanup must be idempotent and retry-safe"
        );
        assert!(
            manager.atlas_built,
            "a sweep that changes nothing must not request an atlas rebuild"
        );
    }

    #[test]
    fn cleanup_phases_match_the_exact_generation_and_sum_to_the_legacy_barrier() {
        let owner = OwnerId::new(9, 1);
        let newer = OwnerId::new(9, 2);
        let mut manager = FontManager::new(FakeBackend::default());
        manager
            .register_memory(
                owner,
                "generation-exact",
                16.0,
                b"font",
                FontConfig::default(),
                Some(Box::new(|_, _| {})),
            )
            .unwrap_or_else(|error| panic!("registration failed: {error}"));
        manager
            .register_memory(owner, "claimed", 16.0, b"font", FontConfig::default(), None)
            .unwrap_or_else(|error| panic!("claim registration failed: {error}"));

        assert_eq!(manager.cleanup_owner_callbacks(newer), 0);
        assert_eq!(manager.cleanup_owner_resources(newer), 0);
        assert_eq!(
            manager.len(),
            2,
            "a newer generation of the same signature must not clean the older one"
        );

        assert_eq!(
            manager.cleanup_owner(owner),
            2,
            "the combined barrier must still report claim and subscriber removals"
        );
        assert!(manager.is_empty());
    }

    #[test]
    fn get_matches_legacy_immediate_callback_behavior() {
        let observed = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&observed);
        let mut manager = FontManager::new(FakeBackend::default());
        let result = manager
            .get(
                OwnerId::new(9, 1),
                "missing",
                Box::new(move |_, font| sink.borrow_mut().push(font.is_some())),
            )
            .unwrap_or_else(|error| panic!("get failed: {error}"));
        assert!(result.subscription.is_none());
        assert_eq!(&*observed.borrow(), &[false]);
    }

    #[test]
    fn resize_only_invalidates_on_a_real_change() {
        let mut manager = FontManager::new(FakeBackend::default());
        assert!(
            manager
                .register_memory(
                    OwnerId::HOST,
                    "FONT_DEFAULT",
                    15.0,
                    b"font",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert_eq!(manager.resize("FONT_DEFAULT", 15.0), Ok(false));
        assert_eq!(manager.resize("FONT_DEFAULT", 18.0), Ok(true));
    }

    #[derive(Clone, Debug, PartialEq)]
    struct EntrySnapshot {
        identifier: Vec<u8>,
        size: f32,
        data: Vec<u8>,
        config: FontConfig,
        handle: Option<FontHandle>,
        owner_claims: Vec<OwnerId>,
        subscriber_owners: Vec<OwnerId>,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct ManagerSnapshot {
        entries: Vec<EntrySnapshot>,
        atlas_built: bool,
        next_subscription: u64,
        backend_generations: usize,
    }

    fn snapshot(manager: &FontManager<FakeBackend>) -> ManagerSnapshot {
        ManagerSnapshot {
            entries: manager
                .registry
                .iter()
                .map(|entry| EntrySnapshot {
                    identifier: entry.identifier.as_bytes().to_vec(),
                    size: entry.size,
                    data: entry.data.clone(),
                    config: entry.config.clone(),
                    handle: entry.handle,
                    owner_claims: entry.owner_claims.iter().copied().collect(),
                    subscriber_owners: entry
                        .subscribers
                        .iter()
                        .map(|subscriber| subscriber.owner)
                        .collect(),
                })
                .collect(),
            atlas_built: manager.atlas_built,
            next_subscription: manager.next_subscription,
            backend_generations: manager.backend.generations,
        }
    }

    #[test]
    fn replacement_preserves_subscribed_default_and_callback_cycle() {
        let host = OwnerId::HOST;
        let foreign = OwnerId::new(41, 1);
        let events = Rc::new(RefCell::new(Vec::new()));
        let callback_events = Rc::clone(&events);
        let mut manager = FontManager::new(FakeBackend::default());
        assert!(
            manager
                .register_memory(
                    host,
                    "FONT_DEFAULT",
                    15.0,
                    b"old default",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(manager.advance(&[]).is_ok());
        assert!(
            manager
                .register_memory(
                    foreign,
                    "FONT_DEFAULT",
                    99.0,
                    b"ignored foreign bytes",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(
            manager
                .get(
                    foreign,
                    "FONT_DEFAULT",
                    Box::new(move |_, handle| {
                        callback_events.borrow_mut().push(handle.is_some());
                    }),
                )
                .is_ok()
        );
        assert_eq!(&*events.borrow(), &[true]);

        let config = FontConfig::default();
        let report = manager
            .replace_owner_memory(
                host,
                &[FontMemoryReplacement {
                    identifier: "FONT_DEFAULT",
                    size: 20.0,
                    data: b"new default",
                    config: &config,
                }],
            )
            .unwrap_or_else(|error| panic!("replacement failed: {error}"));
        assert_eq!(
            report,
            FontOwnerReplaceReport {
                requested: 1,
                created: 0,
                updated: 1,
                removed_claims: 0,
                removed_entries: 0,
                retained_omitted: 0,
            }
        );
        assert_eq!(&*events.borrow(), &[true]);
        assert!(manager.registry[0].owner_claims.contains(&host));
        assert!(manager.registry[0].owner_claims.contains(&foreign));
        assert_eq!(manager.registry[0].subscribers.len(), 1);

        let advance = manager
            .advance(&[])
            .unwrap_or_else(|error| panic!("advance failed: {error}"));
        assert_eq!(
            advance,
            FontAdvance {
                rebuilt: true,
                callback_panics: 0
            }
        );
        assert_eq!(&*events.borrow(), &[true, false, true]);
        assert_eq!(manager.backend.fonts[0].data, b"new default");
        assert_eq!(manager.backend.fonts[0].size, 20.0);
        assert_eq!(manager.advance(&[]), Ok(FontAdvance::default()));
        assert_eq!(&*events.borrow(), &[true, false, true]);
    }

    #[test]
    fn replacement_reorders_owned_fonts_as_contiguous_merge_pairs() {
        let host = OwnerId::HOST;
        let foreign = OwnerId::new(42, 1);
        let mut manager = FontManager::new(FakeBackend::default());
        let merge_config = FontConfig {
            merge_mode: true,
            ..FontConfig::default()
        };
        for (owner, identifier, merge_mode) in [
            (foreign, "FOREIGN_BEFORE", false),
            (host, "B", false),
            (foreign, "FOREIGN_BETWEEN", false),
            (host, "B_MERGE", true),
            (host, "A", false),
            (host, "A_MERGE", true),
        ] {
            let config = if merge_mode {
                merge_config.clone()
            } else {
                FontConfig::default()
            };
            assert!(
                manager
                    .register_memory(owner, identifier, 16.0, b"old", config, None)
                    .is_ok()
            );
        }

        let base_config = FontConfig::default();
        let replacements = [
            FontMemoryReplacement {
                identifier: "A",
                size: 17.0,
                data: b"a",
                config: &base_config,
            },
            FontMemoryReplacement {
                identifier: "A_MERGE",
                size: 17.0,
                data: b"a merge",
                config: &merge_config,
            },
            FontMemoryReplacement {
                identifier: "B",
                size: 18.0,
                data: b"b",
                config: &base_config,
            },
            FontMemoryReplacement {
                identifier: "B_MERGE",
                size: 18.0,
                data: b"b merge",
                config: &merge_config,
            },
        ];
        let report = manager
            .replace_owner_memory(host, &replacements)
            .unwrap_or_else(|error| panic!("replacement failed: {error}"));
        assert_eq!(report.requested, 4);
        assert_eq!(report.updated, 4);
        assert_eq!(report.created, 0);
        assert!(manager.advance(&[]).is_ok());
        let identifiers = manager
            .backend
            .fonts
            .iter()
            .map(|font| font.identifier.as_slice())
            .collect::<Vec<_>>();
        assert_eq!(
            identifiers,
            vec![
                b"FOREIGN_BEFORE".as_slice(),
                b"A".as_slice(),
                b"A_MERGE".as_slice(),
                b"B".as_slice(),
                b"B_MERGE".as_slice(),
                b"FOREIGN_BETWEEN".as_slice(),
            ]
        );
        assert_eq!(
            manager
                .backend
                .fonts
                .iter()
                .map(|font| font.merge_mode)
                .collect::<Vec<_>>(),
            vec![false, false, true, false, true, false]
        );
    }

    #[test]
    fn replacement_preflight_errors_leave_registry_completely_unchanged() {
        let host = OwnerId::HOST;
        let foreign = OwnerId::new(43, 1);
        let mut manager = FontManager::new(FakeBackend::default());
        assert!(
            manager
                .register_memory(
                    host,
                    "HOST_FONT",
                    15.0,
                    b"host",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(
            manager
                .register_memory(
                    foreign,
                    "FOREIGN_FONT",
                    16.0,
                    b"foreign",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(manager.advance(&[]).is_ok());
        let before = snapshot(&manager);
        let config = FontConfig::default();

        let invalid = [
            FontMemoryReplacement {
                identifier: "VALID_FIRST",
                size: 17.0,
                data: b"valid",
                config: &config,
            },
            FontMemoryReplacement {
                identifier: "INVALID_SECOND",
                size: 17.0,
                data: b"",
                config: &config,
            },
        ];
        assert_eq!(
            manager.replace_owner_memory(host, &invalid),
            Err(FontOwnerReplaceError::InvalidRequest {
                request_index: 1,
                source: super::FontError::EmptyData,
            })
        );
        assert_eq!(snapshot(&manager), before);

        let duplicate = [
            FontMemoryReplacement {
                identifier: "DUPLICATE",
                size: 17.0,
                data: b"one",
                config: &config,
            },
            FontMemoryReplacement {
                identifier: "DUPLICATE",
                size: 18.0,
                data: b"two",
                config: &config,
            },
        ];
        assert_eq!(
            manager.replace_owner_memory(host, &duplicate),
            Err(FontOwnerReplaceError::DuplicateIdentifier { request_index: 1 })
        );
        assert_eq!(snapshot(&manager), before);

        assert_eq!(
            manager.replace_owner_memory(
                host,
                &[FontMemoryReplacement {
                    identifier: "FOREIGN_FONT",
                    size: 18.0,
                    data: b"conflict",
                    config: &config,
                }],
            ),
            Err(FontOwnerReplaceError::OwnerConflict { request_index: 0 })
        );
        assert_eq!(snapshot(&manager), before);
    }

    #[test]
    fn replacement_removes_only_owner_claims_and_retains_foreign_entries() {
        let host = OwnerId::HOST;
        let foreign = OwnerId::new(44, 1);
        let mut manager = FontManager::new(FakeBackend::default());
        assert!(
            manager
                .register_memory(host, "SHARED", 15.0, b"shared", FontConfig::default(), None,)
                .is_ok()
        );
        assert!(
            manager
                .register_memory(
                    foreign,
                    "SHARED",
                    99.0,
                    b"ignored",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(
            manager
                .register_memory(
                    host,
                    "HOST_ONLY",
                    16.0,
                    b"host only",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(manager.advance(&[]).is_ok());

        let report = manager
            .replace_owner_memory(host, &[])
            .unwrap_or_else(|error| panic!("replacement failed: {error}"));
        assert_eq!(
            report,
            FontOwnerReplaceReport {
                requested: 0,
                created: 0,
                updated: 0,
                removed_claims: 2,
                removed_entries: 1,
                retained_omitted: 1,
            }
        );
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.registry[0].identifier.as_bytes(), b"SHARED");
        assert!(!manager.registry[0].owner_claims.contains(&host));
        assert!(manager.registry[0].owner_claims.contains(&foreign));
        let advance = manager
            .advance(&[])
            .unwrap_or_else(|error| panic!("advance failed: {error}"));
        assert!(advance.rebuilt);
        assert_eq!(manager.backend.generations, 2);
        assert_eq!(manager.backend.fonts[0].identifier, b"SHARED");
    }
}
