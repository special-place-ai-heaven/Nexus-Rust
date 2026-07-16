use std::fmt;

/// Failure while preparing, installing, accessing, or restoring a shadow vtable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtableError {
    /// The supplied COM interface pointer was null.
    NullInterface,
    /// The declared layout cannot describe a COM interface.
    InvalidLayout {
        /// Human-readable interface layout name.
        layout: &'static str,
        /// Number of entries declared by the layout.
        slot_count: usize,
    },
    /// The interface pointer does not have pointer alignment.
    MisalignedInterface {
        /// Human-readable interface layout name.
        layout: &'static str,
    },
    /// The interface contained a null vtable pointer.
    NullVtable {
        /// Human-readable interface layout name.
        layout: &'static str,
    },
    /// The interface contained a vtable pointer without pointer alignment.
    MisalignedVtable {
        /// Human-readable interface layout name.
        layout: &'static str,
    },
    /// A required vtable entry was null.
    NullEntry {
        /// Human-readable interface layout name.
        layout: &'static str,
        /// Zero-based entry index.
        index: usize,
    },
    /// A typed method refers past the declared layout.
    SlotOutOfBounds {
        /// Human-readable interface layout name.
        layout: &'static str,
        /// Human-readable method name.
        method: &'static str,
        /// Zero-based method index.
        index: usize,
        /// Number of entries declared by the layout.
        slot_count: usize,
    },
    /// A method type is not represented by exactly one pointer.
    InvalidMethodRepresentation {
        /// Human-readable method name.
        method: &'static str,
        /// Size of the method type in bytes.
        actual_size: usize,
    },
    /// A replacement method encoded as a null pointer.
    NullReplacement {
        /// Human-readable method name.
        method: &'static str,
    },
    /// The object no longer used the vtable from which the shadow was prepared.
    VtableChanged {
        /// Human-readable interface layout name.
        layout: &'static str,
    },
    /// Another component replaced the installed shadow before restoration.
    VtableDisplaced {
        /// Human-readable interface layout name.
        layout: &'static str,
    },
}

impl fmt::Display for VtableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullInterface => formatter.write_str("the COM interface pointer is null"),
            Self::InvalidLayout { layout, slot_count } => {
                write!(
                    formatter,
                    "{layout} declares an invalid {slot_count}-entry vtable"
                )
            }
            Self::MisalignedInterface { layout } => {
                write!(formatter, "the {layout} interface pointer is misaligned")
            }
            Self::NullVtable { layout } => {
                write!(formatter, "the {layout} interface has a null vtable")
            }
            Self::MisalignedVtable { layout } => {
                write!(formatter, "the {layout} vtable pointer is misaligned")
            }
            Self::NullEntry { layout, index } => {
                write!(formatter, "{layout} vtable entry {index} is null")
            }
            Self::SlotOutOfBounds {
                layout,
                method,
                index,
                slot_count,
            } => write!(
                formatter,
                "{method} uses slot {index}, outside the {slot_count}-entry {layout} vtable"
            ),
            Self::InvalidMethodRepresentation {
                method,
                actual_size,
            } => write!(
                formatter,
                "{method} has a {actual_size}-byte representation instead of one pointer"
            ),
            Self::NullReplacement { method } => {
                write!(formatter, "the replacement for {method} is null")
            }
            Self::VtableChanged { layout } => {
                write!(formatter, "the {layout} vtable changed before installation")
            }
            Self::VtableDisplaced { layout } => {
                write!(formatter, "the installed {layout} shadow was displaced")
            }
        }
    }
}

impl std::error::Error for VtableError {}
