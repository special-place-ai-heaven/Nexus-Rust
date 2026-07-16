use std::collections::VecDeque;

use nexus_core::OwnerToken;

/// Diagnostic severity without caller-controlled text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    /// Expected lifecycle information.
    Info,
    /// Recoverable policy or lifecycle issue.
    Warning,
    /// Operation failed or a native generation was contained.
    Error,
}

/// Closed diagnostic category. No variant carries a path, address, URL, or panic payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    /// A new inert DLL candidate was discovered.
    CandidateDiscovered,
    /// A candidate revision changed.
    CandidateChanged,
    /// A candidate disappeared from the directory.
    CandidateRemoved,
    /// A candidate moved within the directory.
    CandidateRenamed,
    /// A native definition was inspected and admitted.
    DefinitionInspected,
    /// Policy blocked activation.
    PolicyBlocked,
    /// Native activation completed.
    Activated,
    /// Callback ingress was closed for unload.
    UnloadRequested,
    /// Callback and registration drain completed.
    DrainComplete,
    /// The native unload callback completed or was contained.
    NativeUnloadComplete,
    /// The module reference was released.
    ModuleReleased,
    /// An activation failure was contained.
    ActivationFailed,
    /// A cleanup phase must be retried.
    CleanupRetryRequired,
    /// A failed module release may be retried.
    ReleaseRetryRequired,
    /// A module release outcome is uncertain and was pinned.
    ReleasePinned,
    /// A runtime hot reload completed.
    HotReloadComplete,
    /// An injected boundary panicked and its payload was discarded.
    BoundaryPanicContained,
}

/// One redaction-safe lifecycle diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    sequence: u64,
    severity: DiagnosticSeverity,
    code: DiagnosticCode,
    owner: Option<OwnerToken>,
}

impl Diagnostic {
    /// Returns the monotonic per-manager sequence number.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the closed severity.
    #[must_use]
    pub const fn severity(self) -> DiagnosticSeverity {
        self.severity
    }

    /// Returns the closed diagnostic code.
    #[must_use]
    pub const fn code(self) -> DiagnosticCode {
        self.code
    }

    /// Returns the exact generation when the diagnostic concerns one.
    #[must_use]
    pub const fn owner(self) -> Option<OwnerToken> {
        self.owner
    }
}

pub(crate) struct DiagnosticBuffer {
    capacity: usize,
    next_sequence: u64,
    entries: VecDeque<Diagnostic>,
}

impl DiagnosticBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            next_sequence: 1,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    pub(crate) fn push(
        &mut self,
        severity: DiagnosticSeverity,
        code: DiagnosticCode,
        owner: Option<OwnerToken>,
    ) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(Diagnostic {
            sequence: self.next_sequence,
            severity,
            code,
            owner,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub(crate) fn entries(&self) -> &VecDeque<Diagnostic> {
        &self.entries
    }

    pub(crate) fn take(&mut self) -> Vec<Diagnostic> {
        self.entries.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{DiagnosticBuffer, DiagnosticCode, DiagnosticSeverity};

    #[test]
    fn bounded_buffer_drops_oldest_without_unbounded_growth() {
        let mut buffer = DiagnosticBuffer::new(2);
        for _ in 0..3 {
            buffer.push(
                DiagnosticSeverity::Info,
                DiagnosticCode::CandidateDiscovered,
                None,
            );
        }
        let entries = buffer.take();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence(), 2);
        assert_eq!(entries[1].sequence(), 3);
    }
}
