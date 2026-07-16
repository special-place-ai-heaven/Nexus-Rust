use std::panic::{AssertUnwindSafe, catch_unwind};

/// One scalar Win32 message routed through the Nexus compatibility pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowMessage {
    /// Opaque same-process window token.
    pub window: usize,
    /// Native message identifier.
    pub message: u32,
    /// Native unsigned parameter.
    pub wparam: usize,
    /// Native signed parameter.
    pub lparam: isize,
}

/// Result of one compatibility routing stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowMessageRoute {
    /// Continue routing, optionally with a translated message identifier.
    Continue(WindowMessage),
    /// Consume the message without invoking later stages or the game WndProc.
    Consume,
}

/// Injected compatibility stages that surround the overlay's UI input stage.
///
/// The overlay always calls these in legacy order: `before_ui`, UI enqueue and
/// capture, `after_ui`, runtime shutdown handling, and `redirect_game_only`.
/// The predecessor WndProc is then called exactly once unless a stage consumes.
pub trait WindowMessageRouter: Send + Sync + 'static {
    /// Routes directory notifications and addon raw callbacks before the UI.
    fn before_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        WindowMessageRoute::Continue(message)
    }

    /// Routes managed input bindings after the UI has declined the message.
    fn after_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        WindowMessageRoute::Continue(message)
    }

    /// Translates Nexus game-only messages immediately before game dispatch.
    fn redirect_game_only(&self, message: WindowMessage) -> WindowMessage {
        message
    }
}

/// Router used by render-probe and isolated overlay configurations.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopWindowMessageRouter;

impl WindowMessageRouter for NoopWindowMessageRouter {}

pub(crate) fn route_safely(
    router: &dyn WindowMessageRouter,
    message: WindowMessage,
    stage: RouteStage,
) -> WindowMessageRoute {
    catch_unwind(AssertUnwindSafe(|| match stage {
        RouteStage::BeforeUi => router.before_ui(message),
        RouteStage::AfterUi => router.after_ui(message),
    }))
    .unwrap_or(WindowMessageRoute::Continue(message))
}

pub(crate) fn redirect_safely(
    router: &dyn WindowMessageRouter,
    message: WindowMessage,
) -> WindowMessage {
    catch_unwind(AssertUnwindSafe(|| router.redirect_game_only(message))).unwrap_or(message)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteStage {
    BeforeUi,
    AfterUi,
}

#[cfg(test)]
mod tests {
    use super::{
        RouteStage, WindowMessage, WindowMessageRoute, WindowMessageRouter, redirect_safely,
        route_safely,
    };

    fn message(value: u32) -> WindowMessage {
        WindowMessage {
            window: 1,
            message: value,
            wparam: 2,
            lparam: 3,
        }
    }

    struct Router;

    impl WindowMessageRouter for Router {
        fn before_ui(&self, message: WindowMessage) -> WindowMessageRoute {
            WindowMessageRoute::Continue(WindowMessage {
                message: message.message + 1,
                ..message
            })
        }

        fn after_ui(&self, _message: WindowMessage) -> WindowMessageRoute {
            WindowMessageRoute::Consume
        }

        fn redirect_game_only(&self, message: WindowMessage) -> WindowMessage {
            WindowMessage {
                message: message.message + 10,
                ..message
            }
        }
    }

    #[test]
    fn stages_preserve_transform_and_consume_semantics() {
        assert_eq!(
            route_safely(&Router, message(4), RouteStage::BeforeUi),
            WindowMessageRoute::Continue(message(5))
        );
        assert_eq!(
            route_safely(&Router, message(4), RouteStage::AfterUi),
            WindowMessageRoute::Consume
        );
        assert_eq!(redirect_safely(&Router, message(4)), message(14));
    }

    struct Panics;

    impl WindowMessageRouter for Panics {
        fn before_ui(&self, _message: WindowMessage) -> WindowMessageRoute {
            panic!("test router panic")
        }

        fn redirect_game_only(&self, _message: WindowMessage) -> WindowMessage {
            panic!("test redirect panic")
        }
    }

    #[test]
    fn router_panics_fail_open_without_crossing_wndproc() {
        assert_eq!(
            route_safely(&Panics, message(7), RouteStage::BeforeUi),
            WindowMessageRoute::Continue(message(7))
        );
        assert_eq!(redirect_safely(&Panics, message(7)), message(7));
    }
}
