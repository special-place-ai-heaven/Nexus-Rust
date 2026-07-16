//! GPU-independent render policy for Nexus.
//!
//! This crate owns decisions about which observed swap chain is the game,
//! when render resources need a new generation, and when a failing render
//! stage must cool down or be disabled. Platform adapters translate native
//! D3D/DXGI values into these types; this crate performs no GPU calls.

#![forbid(unsafe_code)]

mod classifier;
mod observation;
mod safety;
mod session;

pub use classifier::{
    CandidateRejection, Classification, ClassifierConfig, NoSelectionReason, OverrideStatus,
    PrimarySwapChainClassifier, RejectionReason, SelectionReason,
};
pub use observation::{
    Activity, AdapterLuid, ColorSpace, DeviceId, Extent2D, Hwnd, PresentMethod, SurfaceFormat,
    SwapChainId, SwapChainObservation,
};
pub use safety::{
    AttemptPermission, FailureAction, FailureController, FailurePolicy, RenderControls,
    RenderStage, SafeMode, StageFailureState,
};
pub use session::{
    GenerationChange, SessionEvent, SessionEventKind, SessionGeneration, SessionLifecycle,
    SwapChainRegistry, SwapChainSession,
};
