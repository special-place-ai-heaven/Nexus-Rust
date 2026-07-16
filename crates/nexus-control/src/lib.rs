#![doc = "Typed runtime controls and redaction-safe diagnostics for Nexus."]
#![forbid(unsafe_code)]

mod command_line;
mod diagnostics;

pub use command_line::{
    AddonSelection, CommandLineParse, ControlConfig, ControlConstraint, ControlIssue,
    ControlOption, HookMode, LegacyOptions, MultiboxOptions, MumbleOption, RedactedArg,
    RuntimeControls, SafeModeStage, UserOverrides, parse_args,
};
pub use diagnostics::{
    AdapterId, AdapterOperation, DiagnosticEvent, FailureCode, InternalFailure, ProxyExport,
    ProxyModule, ProxyOperation, RenderOperation, SwapChainId, SwapChainOperation,
};
