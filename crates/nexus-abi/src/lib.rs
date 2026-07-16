//! Binary-compatible types shared by Nexus and native add-ons.
//!
//! This crate deliberately contains data layouts only. Owning wrappers and all
//! pointer validation belong in safe higher-level crates.

#![no_std]

mod addon;
mod api;
mod mumble;
mod nexus_link;
mod version;

pub use addon::{
    AddonApi, AddonDefinitionFlags, AddonDefinitionV1, AddonInterfaces, AddonLoad,
    AddonRuntimeFlags, AddonUnload, GetAddonDefinitionV1, UpdateProvider,
};
pub use api::{
    AddContextMenu, AddFontFromFile, AddFontFromMemory, AddFontFromResource, AddShortcut,
    AddSimpleShortcut, AddonApiV1, AddonApiV2, AddonApiV3, AddonApiV4, AddonApiV5, AddonApiV6,
    ChangeHook, CreateHook, DataLinkVTable, DeregisterCloseOnEscape, DeregisterInputBind,
    DeregisterRender, EventCallback, EventsVTable, FontsVTable, GameBind, GameBindsVTable,
    GetAddonDirectory, GetCommonDirectory, GetDataLinkResource, GetGameDirectory,
    GetOrCreateTextureFromFile, GetOrCreateTextureFromMemory, GetOrCreateTextureFromResource,
    GetOrCreateTextureFromUrl, GetOrReleaseFont, GetTexture, InputBindCallbackV1,
    InputBindCallbackV2, InputBindV1, InputBindsVTable, InvokeGameBind, InvokeInputBind,
    IsGameBindBound, LoadTextureFromFile, LoadTextureFromMemory, LoadTextureFromResource,
    LoadTextureFromUrl, LocalizationVTable, Log, LogLevel, LogV1, MinHookStatus, MinHookVTable,
    PathsVTable, PressGameBind, QuickAccessGeneric, QuickAccessVTable, RaiseEvent,
    RaiseEventNotification, RaiseEventNotificationTargeted, RaiseEventTargeted, ReceiveFont,
    ReceiveTexture, RegisterCloseOnEscape, RegisterInputBindStringV1, RegisterInputBindStringV2,
    RegisterInputBindStructV1, RegisterInputBindStructV2, RegisterRender, RegisterWndProc,
    RenderCallback, RenderPhase, RendererVTable, RequestUpdate, ResizeFont, SendAlert,
    SendWndProcToGame, SetTranslatedString, ShareDataLinkResource, SubscribeEvent, Texture,
    TexturesVTable, Translate, TranslateTo, UiVTable, WndProcCallback, WndProcVTable,
};
pub use mumble::{
    DEFAULT_MUMBLE_MAPPING_NAME, DL_MUMBLE_LINK, DL_MUMBLE_LINK_IDENTITY,
    EV_MUMBLE_IDENTITY_UPDATED, MumbleCompass, MumbleContext, MumbleContextFlags, MumbleData,
    MumbleIdentity, MumbleMapType, MumbleMountIndex, MumbleProfession, MumbleRace,
    MumbleServerAddress, MumbleUiScale, MumbleVector2, MumbleVector3,
};
pub use nexus_link::NexusLinkData;
pub use version::{ParseVersionError, Version};
