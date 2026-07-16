use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sentinel(u32);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Capture(StateStep),
    Restore(StateStep, u32),
    DropReport(Vec<StateStep>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MockError(StateStep);

#[derive(Default)]
struct MockBackend {
    events: Vec<Event>,
    fail_capture: Vec<StateStep>,
    fail_restore: Vec<StateStep>,
    panic_restore: Vec<StateStep>,
    unobservable_stream_output: bool,
}

impl MockBackend {
    fn capture<T>(&mut self, step: StateStep, value: T) -> Result<T, MockError> {
        self.events.push(Event::Capture(step));
        if self.fail_capture.contains(&step) {
            Err(MockError(step))
        } else {
            Ok(value)
        }
    }

    fn restore(&mut self, step: StateStep, marker: u32) -> Result<(), MockError> {
        self.events.push(Event::Restore(step, marker));
        assert!(
            !self.panic_restore.contains(&step),
            "configured restore panic"
        );
        if self.fail_restore.contains(&step) {
            Err(MockError(step))
        } else {
            Ok(())
        }
    }
}

impl CriticalPipelineBackend for MockBackend {
    type Handle = Sentinel;
    type Error = MockError;

    fn capture_output_merger(&mut self) -> Result<OutputMergerState<Self::Handle>, Self::Error> {
        self.capture(StateStep::OutputMerger, output_merger(10))
    }

    fn capture_input_assembler(
        &mut self,
    ) -> Result<InputAssemblerState<Self::Handle>, Self::Error> {
        self.capture(StateStep::InputAssembler, input_assembler(20))
    }

    fn capture_rasterizer(&mut self) -> Result<RasterizerState<Self::Handle>, Self::Error> {
        self.capture(StateStep::Rasterizer, rasterizer(30))
    }

    fn capture_vertex_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture(StateStep::VertexShader, shader_stage(40))
    }

    fn capture_pixel_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture(StateStep::PixelShader, shader_stage(50))
    }

    fn restore_output_merger(
        &mut self,
        state: &OutputMergerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::OutputMerger, marker(&state.render_targets[0]))
    }

    fn restore_input_assembler(
        &mut self,
        state: &InputAssemblerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::InputAssembler, marker(&state.input_layout))
    }

    fn restore_rasterizer(
        &mut self,
        state: &RasterizerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::Rasterizer, marker(&state.state))
    }

    fn restore_vertex_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::VertexShader, marker(&state.shader))
    }

    fn restore_pixel_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::PixelShader, marker(&state.shader))
    }

    fn on_drop_restore_failures(&mut self, failures: &RestoreFailures<Self::Error>) {
        self.events.push(Event::DropReport(
            failures
                .failures()
                .iter()
                .map(|failure| failure.step)
                .collect(),
        ));
    }
}

impl ExhaustivePipelineBackend for MockBackend {
    fn capture_hull_shader(&mut self) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture(StateStep::HullShader, shader_stage(60))
    }

    fn capture_domain_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture(StateStep::DomainShader, shader_stage(70))
    }

    fn capture_geometry_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture(StateStep::GeometryShader, shader_stage(80))
    }

    fn capture_compute_shader(&mut self) -> Result<ComputeState<Self::Handle>, Self::Error> {
        self.capture(StateStep::ComputeShader, compute_state(90))
    }

    fn capture_stream_output(&mut self) -> Result<StreamOutputState<Self::Handle>, Self::Error> {
        let mut state = stream_output(100);
        if self.unobservable_stream_output {
            state.offsets = StreamOutputOffsets::Unobservable;
        }
        self.capture(StateStep::StreamOutput, state)
    }

    fn capture_predication(&mut self) -> Result<PredicationState<Self::Handle>, Self::Error> {
        self.capture(
            StateStep::Predication,
            PredicationState {
                predicate: Some(Sentinel(110)),
                value: true,
            },
        )
    }

    fn restore_stream_output(
        &mut self,
        state: &StreamOutputState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::StreamOutput, marker(&state.targets[0]))
    }

    fn restore_compute_outputs(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(
            StateStep::ComputeOutputs,
            marker(&state.unordered_access_views[0]),
        )
    }

    fn restore_hull_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::HullShader, marker(&state.shader))
    }

    fn restore_domain_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::DomainShader, marker(&state.shader))
    }

    fn restore_geometry_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::GeometryShader, marker(&state.shader))
    }

    fn restore_compute_shader(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::ComputeShader, marker(&state.stage.shader))
    }

    fn restore_predication(
        &mut self,
        state: &PredicationState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore(StateStep::Predication, marker(&state.predicate))
    }
}

fn marker(value: &Option<Sentinel>) -> u32 {
    value.as_ref().map_or(0, |sentinel| sentinel.0)
}

fn slots<const N: usize>(value: u32) -> [Option<Sentinel>; N] {
    std::array::from_fn(|index| (index == 0).then_some(Sentinel(value)))
}

fn shader_stage(value: u32) -> ProgrammableStageState<Sentinel> {
    ProgrammableStageState {
        shader: Some(Sentinel(value)),
        class_instances: vec![Sentinel(value + 1)],
        constant_buffers: slots::<COMMONSHADER_CONSTANT_BUFFER_SLOTS>(value + 2),
        shader_resources: slots::<COMMONSHADER_INPUT_RESOURCE_SLOTS>(value + 3),
        samplers: slots::<COMMONSHADER_SAMPLER_SLOTS>(value + 4),
    }
}

fn output_merger(value: u32) -> OutputMergerState<Sentinel> {
    OutputMergerState {
        render_targets: slots::<OM_RENDER_TARGET_SLOTS>(value),
        depth_stencil_view: Some(Sentinel(value + 1)),
        unordered_access_views: slots::<PS_CS_UAV_SLOTS>(value + 2),
        unordered_access_counters: HiddenCounterState::Preserve,
        blend_state: Some(Sentinel(value + 3)),
        blend_factor: [0.1, 0.2, 0.3, 0.4],
        sample_mask: 0x1234_5678,
        depth_stencil_state: Some(Sentinel(value + 4)),
        stencil_reference: 23,
    }
}

fn input_assembler(value: u32) -> InputAssemblerState<Sentinel> {
    InputAssemblerState {
        input_layout: Some(Sentinel(value)),
        vertex_buffers: std::array::from_fn(|index| VertexBufferBinding {
            buffer: (index == 0).then_some(Sentinel(value + 1)),
            stride: index as u32 + 4,
            offset: index as u32 + 8,
        }),
        index_buffer: IndexBufferBinding {
            buffer: Some(Sentinel(value + 2)),
            format: 42,
            offset: 64,
        },
        primitive_topology: 4,
    }
}

fn rasterizer(value: u32) -> RasterizerState<Sentinel> {
    RasterizerState {
        state: Some(Sentinel(value)),
        viewports: vec![Viewport {
            width: 1920.0,
            height: 1080.0,
            max_depth: 1.0,
            ..Viewport::default()
        }],
        scissor_rects: vec![Rect {
            right: 1920,
            bottom: 1080,
            ..Rect::default()
        }],
    }
}

fn compute_state(value: u32) -> ComputeState<Sentinel> {
    ComputeState {
        stage: shader_stage(value),
        unordered_access_views: slots::<PS_CS_UAV_SLOTS>(value + 5),
        unordered_access_counters: HiddenCounterState::Preserve,
    }
}

fn stream_output(value: u32) -> StreamOutputState<Sentinel> {
    StreamOutputState {
        targets: slots::<SO_BUFFER_SLOTS>(value),
        offsets: StreamOutputOffsets::Tracked([4, 8, 12, 16]),
    }
}

#[test]
fn critical_guard_restores_exact_data_in_hazard_safe_order() {
    let mut backend = MockBackend::default();
    let guard = CriticalStateGuard::capture(&mut backend)
        .expect("all mock captures are configured to succeed");
    assert_eq!(marker(&guard.state().output_merger.render_targets[0]), 10);
    assert_eq!(marker(&guard.state().input_assembler.input_layout), 20);
    assert_eq!(guard.state().rasterizer.viewports[0].width, 1920.0);
    assert_eq!(marker(&guard.state().vertex_shader.shader), 40);
    assert_eq!(marker(&guard.state().pixel_shader.shader), 50);
    guard.restore().expect("all mock restores succeed");

    assert_eq!(
        backend.events,
        vec![
            Event::Capture(StateStep::OutputMerger),
            Event::Capture(StateStep::InputAssembler),
            Event::Capture(StateStep::Rasterizer),
            Event::Capture(StateStep::VertexShader),
            Event::Capture(StateStep::PixelShader),
            Event::Restore(StateStep::OutputMerger, 10),
            Event::Restore(StateStep::InputAssembler, 20),
            Event::Restore(StateStep::Rasterizer, 30),
            Event::Restore(StateStep::VertexShader, 40),
            Event::Restore(StateStep::PixelShader, 50),
        ]
    );
}

#[test]
fn restore_collects_errors_and_continues_without_drop_retry() {
    let mut backend = MockBackend {
        fail_restore: vec![StateStep::InputAssembler, StateStep::PixelShader],
        ..MockBackend::default()
    };
    let guard = CriticalStateGuard::capture(&mut backend)
        .expect("all mock captures are configured to succeed");
    let failures = guard
        .restore()
        .expect_err("two restore steps are configured to fail");
    assert_eq!(failures.failures().len(), 2);
    assert_eq!(failures.failures()[0].step, StateStep::InputAssembler);
    assert_eq!(failures.failures()[1].step, StateStep::PixelShader);
    assert_eq!(
        backend
            .events
            .iter()
            .filter(|event| matches!(event, Event::Restore(_, _)))
            .count(),
        5
    );
}

#[test]
fn restore_catches_panics_and_attempts_remaining_steps() {
    let mut backend = MockBackend {
        panic_restore: vec![StateStep::VertexShader],
        ..MockBackend::default()
    };
    let guard = CriticalStateGuard::capture(&mut backend)
        .expect("all mock captures are configured to succeed");
    let failures = guard
        .restore()
        .expect_err("the configured restore panic is reported");
    assert_eq!(failures.failures().len(), 1);
    assert_eq!(failures.failures()[0].step, StateStep::VertexShader);
    assert!(matches!(
        failures.failures()[0].cause,
        FailureCause::Panicked
    ));
    assert_eq!(
        backend.events.last(),
        Some(&Event::Restore(StateStep::PixelShader, 50))
    );
}

#[test]
fn drop_restores_and_reports_failures_without_panicking() {
    let mut backend = MockBackend {
        fail_restore: vec![StateStep::Rasterizer],
        ..MockBackend::default()
    };
    {
        let _guard = CriticalStateGuard::capture(&mut backend)
            .expect("all mock captures are configured to succeed");
    }
    assert_eq!(
        backend.events.last(),
        Some(&Event::DropReport(vec![StateStep::Rasterizer]))
    );
}

#[test]
fn capture_failure_identifies_step_and_never_arms_guard() {
    let mut backend = MockBackend {
        fail_capture: vec![StateStep::Rasterizer],
        ..MockBackend::default()
    };
    let failure = match CriticalStateGuard::capture(&mut backend) {
        Ok(_) => panic!("the configured capture failure must be returned"),
        Err(failure) => failure,
    };
    assert_eq!(failure.operation, GuardOperation::Capture);
    assert_eq!(failure.step, StateStep::Rasterizer);
    assert_eq!(
        failure.cause,
        FailureCause::Backend(MockError(StateStep::Rasterizer))
    );
    assert_eq!(backend.events.len(), 3);
    assert!(
        !backend
            .events
            .iter()
            .any(|event| matches!(event, Event::Restore(_, _)))
    );
}

#[test]
fn full_guard_restores_outputs_before_every_input_stage() {
    let mut backend = MockBackend::default();
    let guard =
        FullStateGuard::capture(&mut backend).expect("the exhaustive mock captures every section");
    guard
        .restore()
        .expect("the exhaustive mock restores every section");

    let restore_steps = backend
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Restore(step, _) => Some(*step),
            Event::Capture(_) | Event::DropReport(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        restore_steps,
        vec![
            StateStep::OutputMerger,
            StateStep::StreamOutput,
            StateStep::ComputeOutputs,
            StateStep::InputAssembler,
            StateStep::Rasterizer,
            StateStep::VertexShader,
            StateStep::HullShader,
            StateStep::DomainShader,
            StateStep::GeometryShader,
            StateStep::PixelShader,
            StateStep::ComputeShader,
            StateStep::Predication,
        ]
    );
}

#[test]
fn full_guard_rejects_unobservable_stream_output_offsets() {
    let mut backend = MockBackend {
        unobservable_stream_output: true,
        ..MockBackend::default()
    };
    let failure = match FullStateGuard::capture(&mut backend) {
        Ok(_) => panic!("an unobservable stream-output snapshot is not exhaustive"),
        Err(failure) => failure,
    };
    assert_eq!(failure.step, StateStep::StreamOutput);
    assert_eq!(
        failure.cause,
        FailureCause::IncompleteState(
            "stream-output offsets require an authoritative shadow tracker"
        )
    );
    assert!(
        !backend
            .events
            .contains(&Event::Capture(StateStep::Predication))
    );
}
