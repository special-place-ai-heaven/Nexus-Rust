use core::ptr;
use std::ffi::CString;
use std::sync::Arc;

use nexus_dxgi::{DxgiObservationEvent, RenderCallbackError, RenderSelectionDeferred};
use nexus_imgui_compat::sys;
use nexus_overlay::{UiFrameBuilder, UiFramePreparation};
use nexus_render::{RenderStage, SelectionReason};
use nexus_ui_host::{RenderPhase, UiHost};

const PROBE_TITLE: &[u8] = b"Nexus Rust render probe\0";
const PROBE_TEXT: &[u8] = b"Nexus Rust overlay is rendering\0";
const CORE_TITLE: &[u8] = b"Nexus (Rust)\0";
const CORE_HEADING: &[u8] = b"Rust runtime active\0";
const CORE_RENDER_POLICY: &[u8] = b"Primary Guild Wars 2 swap chain selected\0";
const CORE_NVIDIA_POLICY: &[u8] = b"NVIDIA auxiliary swap chains isolated\0";
const CORE_CONTROL_POLICY: &[u8] = b"Safe modes and hook stages are user-controlled\0";

/// Rust-owned core UI rendered only at the permitted safety stage.
#[derive(Debug)]
pub(crate) struct CoreUiFrameBuilder {
    ui_host: Arc<UiHost>,
    fonts: Arc<crate::fonts::RuntimeFontCoordinator>,
    textures: Arc<crate::textures::RuntimeTextureCoordinator>,
}

impl CoreUiFrameBuilder {
    pub(crate) fn new(
        ui_host: Arc<UiHost>,
        fonts: Arc<crate::fonts::RuntimeFontCoordinator>,
        textures: Arc<crate::textures::RuntimeTextureCoordinator>,
    ) -> Self {
        Self {
            ui_host,
            fonts,
            textures,
        }
    }

    fn invoke_phase(&self, stage: RenderStage, phase: RenderPhase) {
        if stage == RenderStage::Addons {
            let _ = self.ui_host.render().snapshot(phase).invoke_all();
        }
    }

    fn build_core_surface<F>(&self, stage: RenderStage, draw_core: F)
    where
        F: FnOnce(),
    {
        self.invoke_phase(stage, RenderPhase::Render);
        draw_core();
    }
}

impl UiFrameBuilder for CoreUiFrameBuilder {
    fn before_frame(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        if context.is_null() {
            return Ok(());
        }
        advance_addon_frame(
            stage,
            crate::services::advance_localization,
            |localization| {
                self.fonts
                    .advance(context, stage, localization.changed, &localization.texts);
            },
            || {
                let _ = self.textures.advance(stage);
            },
            || self.invoke_phase(stage, RenderPhase::PreRender),
        );
        Ok(())
    }

    fn prepare(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<UiFramePreparation, RenderCallbackError> {
        if context.is_null() {
            return Ok(UiFramePreparation::UNCHANGED);
        }
        if surface_for_stage(stage) == UiSurface::Core {
            apply_ui_services();
        }
        if self.fonts.take_gpu_rebuild(context, stage) {
            Ok(UiFramePreparation::rebuild_font_atlas())
        } else {
            Ok(UiFramePreparation::UNCHANGED)
        }
    }

    fn build(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        if context.is_null() {
            return Ok(());
        }
        match surface_for_stage(stage) {
            UiSurface::None => {}
            UiSurface::Probe => draw_probe(),
            UiSurface::Core => {
                self.build_core_surface(stage, draw_core_status);
            }
        }
        Ok(())
    }

    fn after_render(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        if context.is_null() {
            return Ok(());
        }
        self.invoke_phase(stage, RenderPhase::PostRender);
        Ok(())
    }
}

fn advance_addon_frame<T, L, F, X, P>(
    stage: RenderStage,
    localization: L,
    fonts: F,
    textures: X,
    pre_render: P,
) where
    L: FnOnce() -> T,
    F: FnOnce(T),
    X: FnOnce(),
    P: FnOnce(),
{
    if stage != RenderStage::Addons {
        return;
    }
    let localization = localization();
    fonts(localization);
    textures();
    pre_render();
}

fn apply_ui_services() {
    // SAFETY: the frame builder is invoked only with its owned Dear ImGui 1.80
    // context current, and the IO pointer is not retained past this call.
    let io = unsafe { sys::igGetIO().as_mut() };
    let Some(io) = io else {
        return;
    };
    let width = display_dimension(io.DisplaySize.x);
    let height = display_dimension(io.DisplaySize.y);
    let frame = crate::services::advance_ui_services(width, height);
    io.FontGlobalScale = frame.font_global_scale;
}

fn display_dimension(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 && value <= u32::MAX as f32 {
        value.round() as u32
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiSurface {
    None,
    Probe,
    Core,
}

const fn surface_for_stage(stage: RenderStage) -> UiSurface {
    match stage {
        RenderStage::ProxyOnly | RenderStage::HooksOnly => UiSurface::None,
        RenderStage::RenderProbe => UiSurface::Probe,
        RenderStage::CoreUi | RenderStage::Addons => UiSurface::Core,
    }
}

fn draw_probe() {
    let flags = (sys::ImGuiWindowFlags_AlwaysAutoResize
        | sys::ImGuiWindowFlags_NoSavedSettings
        | sys::ImGuiWindowFlags_NoFocusOnAppearing
        | sys::ImGuiWindowFlags_NoNav) as sys::ImGuiWindowFlags;

    // SAFETY: the overlay invokes this builder only while its exclusively
    // owned Dear ImGui 1.80 context is current. Every byte string is static
    // and NUL-terminated, and Begin is paired with End on every path.
    unsafe {
        sys::igSetNextWindowPos(
            sys::ImVec2 { x: 20.0, y: 20.0 },
            sys::ImGuiCond_Always as sys::ImGuiCond,
            sys::ImVec2 { x: 0.0, y: 0.0 },
        );
        sys::igSetNextWindowBgAlpha(0.92);
        let visible = sys::igBegin(PROBE_TITLE.as_ptr().cast(), ptr::null_mut(), flags);
        if visible {
            text_unformatted(PROBE_TEXT);
        }
        sys::igEnd();
    }
}

fn draw_core_status() {
    let flags = sys::ImGuiWindowFlags_NoFocusOnAppearing as sys::ImGuiWindowFlags;

    // SAFETY: the overlay invokes this builder only while its exclusively
    // owned Dear ImGui 1.80 context is current. Every byte string is static
    // and NUL-terminated, and Begin is paired with End on every path.
    unsafe {
        sys::igSetNextWindowPos(
            sys::ImVec2 { x: 20.0, y: 80.0 },
            sys::ImGuiCond_FirstUseEver as sys::ImGuiCond,
            sys::ImVec2 { x: 0.0, y: 0.0 },
        );
        sys::igSetNextWindowSize(
            sys::ImVec2 { x: 430.0, y: 150.0 },
            sys::ImGuiCond_FirstUseEver as sys::ImGuiCond,
        );
        let visible = sys::igBegin(CORE_TITLE.as_ptr().cast(), ptr::null_mut(), flags);
        if visible {
            text_unformatted(CORE_HEADING);
            sys::igSeparator();
            text_unformatted(CORE_RENDER_POLICY);
            text_unformatted(CORE_NVIDIA_POLICY);
            text_unformatted(CORE_CONTROL_POLICY);
            sys::igSeparator();
            let controls = crate::runtime::runtime_controls();
            text_owned(format!(
                "Hook mode: {} | Safe mode: {}",
                controls.hook_mode, controls.safe_mode
            ));
            text_owned(format!(
                "Lifecycle: {:?}",
                crate::runtime::lifecycle_phase()
            ));
            let observations = crate::dxgi::observation_snapshot();
            text_owned(format!(
                "DXGI observations retained: {}",
                observations.len()
            ));
            if let Some(latest) = observations.last() {
                text_owned(format!("Latest DXGI event: {}", observation_label(latest)));
            }
            match latest_selection_status(&observations) {
                Some(PrimarySelectionStatus::Resolved(reason)) => {
                    text_owned(format!(
                        "Primary selection reason: {}",
                        selection_reason_label(reason)
                    ));
                }
                Some(PrimarySelectionStatus::Deferred(diagnostic)) => {
                    text_owned(format!(
                        "Current primary selection deferral: {}",
                        selection_deferral_label(diagnostic)
                    ));
                }
                None => {}
            }
        }
        sys::igEnd();
    }
}

unsafe fn text_unformatted(text: &'static [u8]) {
    debug_assert_eq!(text.last(), Some(&0));
    // SAFETY: callers provide a static NUL-terminated byte string while the
    // owned Dear ImGui context is current.
    unsafe { sys::igTextUnformatted(text.as_ptr().cast(), ptr::null()) };
}

fn text_owned(text: String) {
    let Ok(text) = CString::new(text) else {
        return;
    };
    // SAFETY: this helper is called only by a frame builder with the owned
    // context current, and the CString remains live for the synchronous call.
    unsafe { sys::igTextUnformatted(text.as_ptr(), ptr::null()) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimarySelectionStatus<'a> {
    Resolved(SelectionReason),
    Deferred(&'a RenderSelectionDeferred),
}

fn latest_selection_status(
    observations: &[DxgiObservationEvent],
) -> Option<PrimarySelectionStatus<'_>> {
    observations
        .iter()
        .enumerate()
        .filter_map(|(arrival, event)| match event {
            DxgiObservationEvent::RenderSelectionResolved {
                sequence,
                resolution,
            } => Some((
                *sequence,
                arrival,
                PrimarySelectionStatus::Resolved(resolution.reason()),
            )),
            DxgiObservationEvent::RenderSelected {
                sequence, reason, ..
            } => Some((
                *sequence,
                arrival,
                PrimarySelectionStatus::Resolved(*reason),
            )),
            DxgiObservationEvent::RenderSelectionDeferred {
                sequence,
                diagnostic,
            } => Some((
                *sequence,
                arrival,
                PrimarySelectionStatus::Deferred(diagnostic),
            )),
            _ => None,
        })
        .max_by_key(|(sequence, arrival, _)| (*sequence, *arrival))
        .map(|(_, _, status)| status)
}

const fn observation_label(event: &DxgiObservationEvent) -> &'static str {
    match event {
        DxgiObservationEvent::FactoryAttached { .. } => "factory attached",
        DxgiObservationEvent::SwapChainAttached { .. } => "swap chain attached",
        DxgiObservationEvent::MetadataIncomplete { .. } => "metadata incomplete",
        DxgiObservationEvent::PresentForwarded { .. } => "present forwarded",
        DxgiObservationEvent::ResizeForwarded { .. } => "resize forwarded",
        DxgiObservationEvent::ColorSpaceForwarded { .. } => "color space changed",
        DxgiObservationEvent::RenderSelectionResolved { .. } => "primary render selection resolved",
        DxgiObservationEvent::RenderSelected { .. } => "primary render selected",
        DxgiObservationEvent::RenderSelectionDeferred { .. } => "primary render selection deferred",
        DxgiObservationEvent::PanicContained { .. } => "panic contained",
        DxgiObservationEvent::Shutdown { .. } => "hooks shut down",
    }
}

fn selection_deferral_label(diagnostic: &RenderSelectionDeferred) -> String {
    diagnostic.to_string()
}

const fn selection_reason_label(reason: SelectionReason) -> &'static str {
    match reason {
        SelectionReason::UserOverride => "user override",
        SelectionReason::OnlyEligibleCandidate => "only eligible chain",
        SelectionReason::ExpectedGameWindow => "expected game window",
        SelectionReason::ForegroundWindow => "foreground window",
        SelectionReason::RetainedPrimary => "retained healthy primary",
        SelectionReason::LargestSurface => "largest surface",
        SelectionReason::LongestActiveStreak => "longest active streak",
        SelectionReason::MostRecentPresentation => "most recent presentation",
        SelectionReason::HighestPresentationCount => "highest presentation count",
        SelectionReason::StableIdentityTieBreak => "stable identity tie-break",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use nexus_dxgi::{
        DxgiObservationEvent, FactoryInterface, RenderSelectionDeferred, RenderSelectionResolved,
    };
    use nexus_imgui_compat::sys;
    use nexus_overlay::UiFrameBuilder;
    use nexus_render::{PrimarySwapChainClassifier, RenderStage, SelectionReason};
    use nexus_ui_host::{OwnerGeneration, RenderPhase, UiCallback, UiHost};

    use crate::fonts::RuntimeFontCoordinator;
    use crate::textures::RuntimeTextureCoordinator;

    use super::{
        CoreUiFrameBuilder, PrimarySelectionStatus, UiSurface, advance_addon_frame,
        latest_selection_status, observation_label, selection_deferral_label,
        selection_reason_label, surface_for_stage,
    };

    fn lock_events<'a>(events: &'a Mutex<Vec<&'static str>>) -> MutexGuard<'a, Vec<&'static str>> {
        match events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_imgui() -> MutexGuard<'static, ()> {
        crate::fonts::IMGUI_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register_event(
        host: &UiHost,
        owner: &nexus_ui_host::OwnerHandle,
        phase: RenderPhase,
        events: &Arc<Mutex<Vec<&'static str>>>,
        label: &'static str,
        panic_after_recording: bool,
    ) {
        let events = Arc::clone(events);
        let callback = UiCallback::managed(owner.clone(), move || {
            lock_events(events.as_ref()).push(label);
            assert!(!panic_after_recording, "intentional callback panic");
        });
        assert!(host.render().register(phase, callback).is_ok());
    }

    fn builder(host: Arc<UiHost>) -> CoreUiFrameBuilder {
        CoreUiFrameBuilder::new(
            host,
            Arc::new(RuntimeFontCoordinator::default()),
            Arc::new(RuntimeTextureCoordinator::default()),
        )
    }

    #[test]
    fn addon_services_advance_in_exact_legacy_order() {
        let events = Mutex::new(Vec::new());
        advance_addon_frame(
            RenderStage::Addons,
            || {
                lock_events(&events).push("localization");
                7
            },
            |value| {
                assert_eq!(value, 7);
                lock_events(&events).push("fonts");
            },
            || lock_events(&events).push("textures"),
            || lock_events(&events).push("pre-render"),
        );
        assert_eq!(
            lock_events(&events).as_slice(),
            ["localization", "fonts", "textures", "pre-render"]
        );

        lock_events(&events).clear();
        advance_addon_frame(
            RenderStage::CoreUi,
            || lock_events(&events).push("localization"),
            |()| lock_events(&events).push("fonts"),
            || lock_events(&events).push("textures"),
            || lock_events(&events).push("pre-render"),
        );
        assert!(lock_events(&events).is_empty());
    }

    #[test]
    fn safety_stages_select_only_the_permitted_surface() {
        assert_eq!(surface_for_stage(RenderStage::ProxyOnly), UiSurface::None);
        assert_eq!(surface_for_stage(RenderStage::HooksOnly), UiSurface::None);
        assert_eq!(
            surface_for_stage(RenderStage::RenderProbe),
            UiSurface::Probe
        );
        assert_eq!(surface_for_stage(RenderStage::CoreUi), UiSurface::Core);
        assert_eq!(surface_for_stage(RenderStage::Addons), UiSurface::Core);
    }

    #[test]
    fn deferred_selection_has_a_stable_ui_label() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let event = DxgiObservationEvent::RenderSelectionDeferred {
            sequence: 1,
            diagnostic: diagnostic.clone(),
        };

        assert_eq!(
            observation_label(&event),
            "primary render selection deferred"
        );
        assert_eq!(
            selection_deferral_label(&diagnostic),
            "no swap-chain observations"
        );
    }

    #[test]
    fn resolved_classification_supersedes_stale_deferral_without_a_render_event() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let resolution = RenderSelectionResolved::new(SelectionReason::ExpectedGameWindow);
        let observations = [
            DxgiObservationEvent::RenderSelectionDeferred {
                sequence: 1,
                diagnostic: diagnostic.clone(),
            },
            DxgiObservationEvent::RenderSelectionResolved {
                sequence: 2,
                resolution,
            },
            DxgiObservationEvent::FactoryAttached {
                interface: FactoryInterface::Base,
            },
        ];

        assert_eq!(
            latest_selection_status(&observations),
            Some(PrimarySelectionStatus::Resolved(
                SelectionReason::ExpectedGameWindow
            ))
        );
        assert_eq!(
            observation_label(&observations[1]),
            "primary render selection resolved"
        );
        assert_eq!(
            selection_reason_label(SelectionReason::ExpectedGameWindow),
            "expected game window"
        );

        let stale_deferred_arrived_late = [
            DxgiObservationEvent::RenderSelectionResolved {
                sequence: 4,
                resolution,
            },
            DxgiObservationEvent::RenderSelectionDeferred {
                sequence: 3,
                diagnostic: diagnostic.clone(),
            },
        ];
        assert_eq!(
            latest_selection_status(&stale_deferred_arrived_late),
            Some(PrimarySelectionStatus::Resolved(
                SelectionReason::ExpectedGameWindow
            ))
        );

        let newer_deferred = [
            DxgiObservationEvent::RenderSelectionResolved {
                sequence: 4,
                resolution,
            },
            DxgiObservationEvent::RenderSelectionDeferred {
                sequence: 5,
                diagnostic,
            },
        ];
        assert!(matches!(
            latest_selection_status(&newer_deferred),
            Some(PrimarySelectionStatus::Deferred(_))
        ));
    }

    #[test]
    fn addon_stage_dispatches_callbacks_at_exact_frame_boundaries() {
        let _imgui_guard = lock_imgui();
        let host = Arc::new(UiHost::default());
        let Ok(owner) = host.owner(OwnerGeneration::new(9, 1)) else {
            panic!("test owner should be active");
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::PreRender,
            &events,
            "pre",
            false,
        );
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::Render,
            &events,
            "render-panics",
            true,
        );
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::Render,
            &events,
            "render",
            false,
        );
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::PostRender,
            &events,
            "post",
            false,
        );

        let builder = builder(Arc::clone(&host));
        // SAFETY: this test creates one Dear ImGui context, uses it only on
        // this thread while current, and destroys it after all phase calls.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        assert!(!context.is_null());

        assert!(builder.before_frame(context, RenderStage::Addons).is_ok());
        assert_eq!(lock_events(events.as_ref()).as_slice(), ["pre"]);

        builder.build_core_surface(RenderStage::Addons, || {
            lock_events(events.as_ref()).push("core");
        });
        assert_eq!(
            lock_events(events.as_ref()).as_slice(),
            ["pre", "render-panics", "render", "core"]
        );
        assert!(builder.after_render(context, RenderStage::Addons).is_ok());
        assert_eq!(
            lock_events(events.as_ref()).as_slice(),
            ["pre", "render-panics", "render", "core", "post"]
        );

        lock_events(events.as_ref()).clear();
        assert!(builder.before_frame(context, RenderStage::CoreUi).is_ok());
        builder.build_core_surface(RenderStage::CoreUi, || {
            lock_events(events.as_ref()).push("core");
        });
        assert!(builder.after_render(context, RenderStage::CoreUi).is_ok());
        assert_eq!(lock_events(events.as_ref()).as_slice(), ["core"]);

        // SAFETY: `context` was created above and remains current and owned by
        // this thread; no callback retained it.
        unsafe { sys::igDestroyContext(context) };
    }

    #[test]
    fn prepare_is_resource_only_between_addon_callback_boundaries() {
        let _imgui_guard = lock_imgui();
        let host = Arc::new(UiHost::default());
        let Ok(owner) = host.owner(OwnerGeneration::new(10, 1)) else {
            panic!("test owner should be active");
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::PreRender,
            &events,
            "pre",
            false,
        );
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::Render,
            &events,
            "render",
            false,
        );
        register_event(
            host.as_ref(),
            &owner,
            RenderPhase::PostRender,
            &events,
            "post",
            false,
        );
        let builder = builder(Arc::clone(&host));

        // SAFETY: this test creates one Dear ImGui context, uses it only on
        // this thread while current, and destroys it before inspecting state.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        assert!(!context.is_null());
        assert!(builder.before_frame(context, RenderStage::Addons).is_ok());
        assert_eq!(lock_events(events.as_ref()).as_slice(), ["pre"]);
        let result = builder.prepare(context, RenderStage::Addons);
        assert_eq!(lock_events(events.as_ref()).as_slice(), ["pre"]);
        builder.build_core_surface(RenderStage::Addons, || {
            lock_events(events.as_ref()).push("core");
        });
        assert_eq!(
            lock_events(events.as_ref()).as_slice(),
            ["pre", "render", "core"]
        );
        assert!(builder.after_render(context, RenderStage::Addons).is_ok());
        // SAFETY: `context` was created above and has not been destroyed or
        // transferred; Dear ImGui accepts it as the current context here.
        unsafe { sys::igDestroyContext(context) };

        assert!(result.is_ok());
        assert_eq!(
            lock_events(events.as_ref()).as_slice(),
            ["pre", "render", "core", "post"]
        );
    }
}
