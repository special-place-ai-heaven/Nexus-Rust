//! Stable ownership tokens used to clean up addon-created UI resources.

use nexus_core::OwnerToken;

/// Identifies the runtime or addon that owns a registration.
///
/// Both the legacy addon signature and its load generation participate in
/// identity. Generations are monotonic per signature, so either field alone
/// can collide with another live addon.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OwnerId {
    signature: u32,
    generation: u64,
}

impl OwnerId {
    /// Ownership token reserved for Nexus itself.
    pub const HOST: Self = Self {
        signature: 0,
        generation: 0,
    };

    /// Creates an addon-generation ownership token.
    #[must_use]
    pub const fn new(signature: u32, generation: u64) -> Self {
        Self {
            signature,
            generation,
        }
    }

    /// Returns the legacy addon signature.
    #[must_use]
    pub const fn signature(self) -> u32 {
        self.signature
    }

    /// Returns the exact load generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

impl From<OwnerToken> for OwnerId {
    fn from(owner: OwnerToken) -> Self {
        Self::new(owner.signature, owner.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::OwnerId;
    use nexus_core::OwnerToken;

    #[test]
    fn signature_and_generation_are_both_identity_dimensions() {
        let first = OwnerId::new(17, 1);
        let reloaded = OwnerId::new(17, 2);
        let other_addon = OwnerId::new(23, 1);

        assert_ne!(first, reloaded);
        assert_ne!(first, other_addon);
        assert_eq!(first.signature(), 17);
        assert_eq!(first.generation(), 1);
        assert_ne!(OwnerId::HOST, first);

        assert_eq!(
            OwnerId::from(OwnerToken {
                signature: 17,
                generation: 1,
            }),
            first
        );
    }
}
