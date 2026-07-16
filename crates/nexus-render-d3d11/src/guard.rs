//! Panic-safe RAII guards and restoration orchestration.

use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use crate::backend::{CriticalPipelineBackend, ExhaustivePipelineBackend};
use crate::model::{CriticalPipelineState, PipelineState, StreamOutputOffsets};

/// Capture or restore operation associated with a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardOperation {
    /// Snapshot capture.
    Capture,
    /// Snapshot restoration.
    Restore,
}

/// Individually observable pipeline section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateStep {
    /// Output-merger state.
    OutputMerger,
    /// Input-assembler state.
    InputAssembler,
    /// Rasterizer state.
    Rasterizer,
    /// Vertex-shader state.
    VertexShader,
    /// Hull-shader state.
    HullShader,
    /// Domain-shader state.
    DomainShader,
    /// Geometry-shader state.
    GeometryShader,
    /// Pixel-shader state.
    PixelShader,
    /// Compute UAV output state.
    ComputeOutputs,
    /// Compute shader and input state.
    ComputeShader,
    /// Stream-output state.
    StreamOutput,
    /// Predication state.
    Predication,
}

/// Cause of a guarded backend failure.
#[derive(Debug, PartialEq, Eq)]
pub enum FailureCause<E> {
    /// Backend returned an error.
    Backend(E),
    /// Backend panicked. The payload is intentionally not retained across the
    /// rendering boundary.
    Panicked,
    /// Backend returned a diagnostic snapshot that cannot be restored exactly.
    IncompleteState(&'static str),
}

/// Failure to capture a state section.
#[derive(Debug, PartialEq, Eq)]
pub struct CaptureFailure<E> {
    /// Operation, always [`GuardOperation::Capture`].
    pub operation: GuardOperation,
    /// State section that failed.
    pub step: StateStep,
    /// Failure cause.
    pub cause: FailureCause<E>,
}

/// Failure to restore one state section.
#[derive(Debug, PartialEq, Eq)]
pub struct RestoreFailure<E> {
    /// Operation, always [`GuardOperation::Restore`].
    pub operation: GuardOperation,
    /// State section that failed.
    pub step: StateStep,
    /// Failure cause.
    pub cause: FailureCause<E>,
}

/// One or more restore failures.
///
/// Restoration is best-effort: all remaining sections are attempted after a
/// failure, and every failure is retained in API order.
#[derive(Debug, PartialEq, Eq)]
pub struct RestoreFailures<E> {
    failures: Vec<RestoreFailure<E>>,
}

impl<E> RestoreFailures<E> {
    /// Individual failures in attempted restore order.
    #[must_use]
    pub fn failures(&self) -> &[RestoreFailure<E>] {
        &self.failures
    }

    /// Consume this value into individual failures.
    #[must_use]
    pub fn into_failures(self) -> Vec<RestoreFailure<E>> {
        self.failures
    }
}

/// RAII guard for the critical Windows backend milestone.
///
/// This guard is deliberately neither `Send` nor `Sync`; it must be restored
/// on the thread that captured its immediate-context state.
pub struct CriticalStateGuard<'backend, B>
where
    B: CriticalPipelineBackend,
{
    backend: &'backend mut B,
    state: Option<CriticalPipelineState<B::Handle>>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<'backend, B> CriticalStateGuard<'backend, B>
where
    B: CriticalPipelineBackend,
{
    /// Capture every state section supported by the critical capability.
    ///
    /// # Errors
    ///
    /// Returns the first backend error or panic and identifies its section.
    pub fn capture(backend: &'backend mut B) -> Result<Self, CaptureFailure<B::Error>> {
        let state = capture_critical(backend)?;
        Ok(Self {
            backend,
            state: Some(state),
            _thread_bound: PhantomData,
        })
    }

    /// Access the backend while the snapshot remains armed.
    pub fn backend_mut(&mut self) -> &mut B {
        self.backend
    }

    /// Inspect the captured snapshot.
    #[must_use]
    pub fn state(&self) -> &CriticalPipelineState<B::Handle> {
        self.state
            .as_ref()
            .expect("an armed guard always contains its snapshot")
    }

    /// Restore explicitly and disarm Drop restoration.
    ///
    /// Every section is attempted even if an earlier section errors or
    /// panics. Output-merger bindings are restored first to resolve overlay
    /// output/input hazards before shader resources are rebound.
    ///
    /// # Errors
    ///
    /// Returns all backend failures and caught panics in attempted order.
    pub fn restore(mut self) -> Result<(), RestoreFailures<B::Error>> {
        let Some(state) = self.state.take() else {
            return Ok(());
        };
        restore_critical(self.backend, &state)
    }
}

impl<B> Drop for CriticalStateGuard<'_, B>
where
    B: CriticalPipelineBackend,
{
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if let Err(failures) = restore_critical(self.backend, &state) {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.backend.on_drop_restore_failures(&failures);
            }));
        }
    }
}

/// RAII guard available only to an exhaustive backend.
///
/// The Windows backend intentionally does not yet implement
/// [`ExhaustivePipelineBackend`], so constructing this guard with
/// `WindowsD3d11Backend` is compile-time unavailable during the interim port.
pub struct FullStateGuard<'backend, B>
where
    B: ExhaustivePipelineBackend,
{
    backend: &'backend mut B,
    state: Option<PipelineState<B::Handle>>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<'backend, B> FullStateGuard<'backend, B>
where
    B: ExhaustivePipelineBackend,
{
    /// Capture the exhaustive pipeline model.
    ///
    /// # Errors
    ///
    /// Returns the first backend error or panic and identifies its section.
    pub fn capture(backend: &'backend mut B) -> Result<Self, CaptureFailure<B::Error>> {
        let critical = capture_critical(backend)?;
        let hull_shader = capture_step(backend, StateStep::HullShader, |backend| {
            backend.capture_hull_shader()
        })?;
        let domain_shader = capture_step(backend, StateStep::DomainShader, |backend| {
            backend.capture_domain_shader()
        })?;
        let geometry_shader = capture_step(backend, StateStep::GeometryShader, |backend| {
            backend.capture_geometry_shader()
        })?;
        let compute_shader = capture_step(backend, StateStep::ComputeShader, |backend| {
            backend.capture_compute_shader()
        })?;
        let stream_output = capture_step(backend, StateStep::StreamOutput, |backend| {
            backend.capture_stream_output()
        })?;
        if matches!(stream_output.offsets, StreamOutputOffsets::Unobservable) {
            return Err(CaptureFailure {
                operation: GuardOperation::Capture,
                step: StateStep::StreamOutput,
                cause: FailureCause::IncompleteState(
                    "stream-output offsets require an authoritative shadow tracker",
                ),
            });
        }
        let predication = capture_step(backend, StateStep::Predication, |backend| {
            backend.capture_predication()
        })?;

        Ok(Self {
            backend,
            state: Some(PipelineState {
                critical,
                hull_shader,
                domain_shader,
                geometry_shader,
                compute_shader,
                stream_output,
                predication,
            }),
            _thread_bound: PhantomData,
        })
    }

    /// Access the backend while the snapshot remains armed.
    pub fn backend_mut(&mut self) -> &mut B {
        self.backend
    }

    /// Inspect the captured snapshot.
    #[must_use]
    pub fn state(&self) -> &PipelineState<B::Handle> {
        self.state
            .as_ref()
            .expect("an armed guard always contains its snapshot")
    }

    /// Restore explicitly and disarm Drop restoration.
    ///
    /// # Errors
    ///
    /// Returns all backend failures and caught panics in attempted order.
    pub fn restore(mut self) -> Result<(), RestoreFailures<B::Error>> {
        let Some(state) = self.state.take() else {
            return Ok(());
        };
        restore_full(self.backend, &state)
    }
}

impl<B> Drop for FullStateGuard<'_, B>
where
    B: ExhaustivePipelineBackend,
{
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if let Err(failures) = restore_full(self.backend, &state) {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.backend.on_drop_restore_failures(&failures);
            }));
        }
    }
}

fn capture_critical<B>(
    backend: &mut B,
) -> Result<CriticalPipelineState<B::Handle>, CaptureFailure<B::Error>>
where
    B: CriticalPipelineBackend,
{
    let output_merger = capture_step(backend, StateStep::OutputMerger, |backend| {
        backend.capture_output_merger()
    })?;
    let input_assembler = capture_step(backend, StateStep::InputAssembler, |backend| {
        backend.capture_input_assembler()
    })?;
    let rasterizer = capture_step(backend, StateStep::Rasterizer, |backend| {
        backend.capture_rasterizer()
    })?;
    let vertex_shader = capture_step(backend, StateStep::VertexShader, |backend| {
        backend.capture_vertex_shader()
    })?;
    let pixel_shader = capture_step(backend, StateStep::PixelShader, |backend| {
        backend.capture_pixel_shader()
    })?;

    Ok(CriticalPipelineState {
        output_merger,
        input_assembler,
        rasterizer,
        vertex_shader,
        pixel_shader,
    })
}

fn capture_step<B, T, E>(
    backend: &mut B,
    step: StateStep,
    operation: impl FnOnce(&mut B) -> Result<T, E>,
) -> Result<T, CaptureFailure<E>> {
    match catch_unwind(AssertUnwindSafe(|| operation(backend))) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CaptureFailure {
            operation: GuardOperation::Capture,
            step,
            cause: FailureCause::Backend(error),
        }),
        Err(_) => Err(CaptureFailure {
            operation: GuardOperation::Capture,
            step,
            cause: FailureCause::Panicked,
        }),
    }
}

fn restore_critical<B>(
    backend: &mut B,
    state: &CriticalPipelineState<B::Handle>,
) -> Result<(), RestoreFailures<B::Error>>
where
    B: CriticalPipelineBackend,
{
    let mut failures = Vec::new();
    attempt_restore(backend, StateStep::OutputMerger, &mut failures, |backend| {
        backend.restore_output_merger(&state.output_merger)
    });
    attempt_restore(
        backend,
        StateStep::InputAssembler,
        &mut failures,
        |backend| backend.restore_input_assembler(&state.input_assembler),
    );
    attempt_restore(backend, StateStep::Rasterizer, &mut failures, |backend| {
        backend.restore_rasterizer(&state.rasterizer)
    });
    attempt_restore(backend, StateStep::VertexShader, &mut failures, |backend| {
        backend.restore_vertex_shader(&state.vertex_shader)
    });
    attempt_restore(backend, StateStep::PixelShader, &mut failures, |backend| {
        backend.restore_pixel_shader(&state.pixel_shader)
    });
    finish_restore(failures)
}

fn restore_full<B>(
    backend: &mut B,
    state: &PipelineState<B::Handle>,
) -> Result<(), RestoreFailures<B::Error>>
where
    B: ExhaustivePipelineBackend,
{
    let mut failures = Vec::new();

    // Restore all outputs before any input resources. D3D11 automatically
    // resolves binding hazards, so this ordering prevents stale overlay
    // outputs from nulling an original shader-resource binding.
    attempt_restore(backend, StateStep::OutputMerger, &mut failures, |backend| {
        backend.restore_output_merger(&state.critical.output_merger)
    });
    attempt_restore(backend, StateStep::StreamOutput, &mut failures, |backend| {
        backend.restore_stream_output(&state.stream_output)
    });
    attempt_restore(
        backend,
        StateStep::ComputeOutputs,
        &mut failures,
        |backend| backend.restore_compute_outputs(&state.compute_shader),
    );
    attempt_restore(
        backend,
        StateStep::InputAssembler,
        &mut failures,
        |backend| backend.restore_input_assembler(&state.critical.input_assembler),
    );
    attempt_restore(backend, StateStep::Rasterizer, &mut failures, |backend| {
        backend.restore_rasterizer(&state.critical.rasterizer)
    });
    attempt_restore(backend, StateStep::VertexShader, &mut failures, |backend| {
        backend.restore_vertex_shader(&state.critical.vertex_shader)
    });
    attempt_restore(backend, StateStep::HullShader, &mut failures, |backend| {
        backend.restore_hull_shader(&state.hull_shader)
    });
    attempt_restore(backend, StateStep::DomainShader, &mut failures, |backend| {
        backend.restore_domain_shader(&state.domain_shader)
    });
    attempt_restore(
        backend,
        StateStep::GeometryShader,
        &mut failures,
        |backend| backend.restore_geometry_shader(&state.geometry_shader),
    );
    attempt_restore(backend, StateStep::PixelShader, &mut failures, |backend| {
        backend.restore_pixel_shader(&state.critical.pixel_shader)
    });
    attempt_restore(
        backend,
        StateStep::ComputeShader,
        &mut failures,
        |backend| backend.restore_compute_shader(&state.compute_shader),
    );
    attempt_restore(backend, StateStep::Predication, &mut failures, |backend| {
        backend.restore_predication(&state.predication)
    });
    finish_restore(failures)
}

fn attempt_restore<B, E>(
    backend: &mut B,
    step: StateStep,
    failures: &mut Vec<RestoreFailure<E>>,
    operation: impl FnOnce(&mut B) -> Result<(), E>,
) {
    let cause = match catch_unwind(AssertUnwindSafe(|| operation(backend))) {
        Ok(Ok(())) => return,
        Ok(Err(error)) => FailureCause::Backend(error),
        Err(_) => FailureCause::Panicked,
    };
    failures.push(RestoreFailure {
        operation: GuardOperation::Restore,
        step,
        cause,
    });
}

fn finish_restore<E>(failures: Vec<RestoreFailure<E>>) -> Result<(), RestoreFailures<E>> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(RestoreFailures { failures })
    }
}
