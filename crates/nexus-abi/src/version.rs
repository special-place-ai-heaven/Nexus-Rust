use core::{fmt, str::FromStr};

/// Four-component version used by the Nexus add-on ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Version {
    /// Major version component.
    pub major: u16,
    /// Minor version component.
    pub minor: u16,
    /// Build version component.
    pub build: u16,
    /// Optional revision component. Zero is omitted when formatted.
    pub revision: u16,
}

impl Version {
    /// Constructs a version from its four ABI components.
    #[must_use]
    pub const fn new(major: u16, minor: u16, build: u16, revision: u16) -> Self {
        Self {
            major,
            minor,
            build,
            revision,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.build)?;
        if self.revision > 0 {
            write!(formatter, ".{}", self.revision)?;
        }
        Ok(())
    }
}

/// Error returned when a Nexus version string is malformed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseVersionError {
    /// A version must contain exactly three or four components.
    InvalidComponentCount,
    /// A component was empty or contained a non-decimal character.
    InvalidComponent {
        /// Zero-based index of the invalid component.
        index: usize,
    },
    /// A decimal component did not fit in the ABI's `u16` field.
    ComponentOutOfRange {
        /// Zero-based index of the overflowing component.
        index: usize,
    },
}

impl fmt::Display for ParseVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidComponentCount => {
                formatter.write_str("a Nexus version requires three or four components")
            }
            Self::InvalidComponent { index } => {
                write!(
                    formatter,
                    "version component {index} is not an unsigned decimal integer"
                )
            }
            Self::ComponentOutOfRange { index } => {
                write!(formatter, "version component {index} exceeds u16")
            }
        }
    }
}

impl core::error::Error for ParseVersionError {}

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.strip_prefix('v').unwrap_or(input);
        let mut values = [0_u16; 4];
        let mut count = 0_usize;

        for (index, component) in input.split('.').enumerate() {
            if index >= values.len() {
                return Err(ParseVersionError::InvalidComponentCount);
            }
            if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ParseVersionError::InvalidComponent { index });
            }

            values[index] = component
                .parse::<u16>()
                .map_err(|_| ParseVersionError::ComponentOutOfRange { index })?;
            count += 1;
        }

        if !(3..=4).contains(&count) {
            return Err(ParseVersionError::InvalidComponentCount);
        }

        Ok(Self::new(values[0], values[1], values[2], values[3]))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::mem::{align_of, size_of};
    use std::{format, string::ToString};

    use super::{ParseVersionError, Version};

    #[test]
    fn layout_matches_cpp_version_t() {
        assert_eq!(size_of::<Version>(), 8);
        assert_eq!(align_of::<Version>(), 2);
    }

    #[test]
    fn parses_tag_and_release_forms() -> Result<(), ParseVersionError> {
        assert_eq!(
            "v2026.2.17.1210".parse::<Version>()?,
            Version::new(2026, 2, 17, 1210)
        );
        assert_eq!(
            "2026.2.17".parse::<Version>()?,
            Version::new(2026, 2, 17, 0)
        );
        Ok(())
    }

    #[test]
    fn formatting_matches_cpp_string_method() {
        assert_eq!(Version::new(2026, 2, 17, 0).to_string(), "2026.2.17");
        assert_eq!(
            format!("{}", Version::new(2026, 2, 17, 1210)),
            "2026.2.17.1210"
        );
    }

    #[test]
    fn comparison_is_lexicographic_by_all_components() {
        assert!(Version::new(2, 0, 0, 0) > Version::new(1, u16::MAX, u16::MAX, u16::MAX));
        assert!(Version::new(1, 2, 3, 5) > Version::new(1, 2, 3, 4));
    }

    #[test]
    fn rejects_shapes_the_cpp_constructor_cannot_build() {
        assert_eq!(
            "1.2".parse::<Version>(),
            Err(ParseVersionError::InvalidComponentCount)
        );
        assert_eq!(
            "1.2.3.4.5".parse::<Version>(),
            Err(ParseVersionError::InvalidComponentCount)
        );
        assert_eq!(
            "1.two.3".parse::<Version>(),
            Err(ParseVersionError::InvalidComponent { index: 1 })
        );
        assert_eq!(
            "1.2.65536".parse::<Version>(),
            Err(ParseVersionError::ComponentOutOfRange { index: 2 })
        );
    }
}
