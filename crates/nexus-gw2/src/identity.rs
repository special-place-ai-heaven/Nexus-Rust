use nexus_abi::{MumbleIdentity, MumbleProfession, MumbleRace, MumbleUiScale};
use serde::Deserialize;
use thiserror::Error;

const IDENTITY_NAME_CAPACITY: usize = 20;

/// Redaction-safe failure categories for Mumble identity parsing.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IdentityParseError {
    /// No identity document was present.
    #[error("the Mumble identity document is empty")]
    Empty,
    /// The shared UTF-16 buffer contains an invalid sequence.
    #[error("the Mumble identity document is not valid UTF-16")]
    InvalidUtf16,
    /// The decoded document is not the expected JSON object.
    #[error("the Mumble identity document is not valid identity JSON")]
    InvalidJson,
    /// The UTF-8 name would not fit the legacy fixed-size array.
    #[error("the Mumble identity name exceeds the legacy ABI capacity")]
    NameTooLong,
    /// A name contained an interior nul byte.
    #[error("the Mumble identity name contains an embedded nul")]
    EmbeddedNul,
}

#[derive(Deserialize)]
struct IdentityDocument {
    name: String,
    profession: u8,
    spec: u32,
    race: u8,
    map_id: u32,
    world_id: u32,
    team_color_id: u32,
    commander: bool,
    fov: f32,
    uisz: u8,
}

/// Parses one nul-terminated UTF-16 identity document atomically.
///
/// Unlike the legacy implementation, all fields are validated before any
/// caller-visible identity is changed and an oversized name cannot overflow
/// the fixed C array.
pub fn parse_identity(units: &[u16]) -> Result<MumbleIdentity, IdentityParseError> {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    if end == 0 {
        return Err(IdentityParseError::Empty);
    }

    let json = String::from_utf16(&units[..end]).map_err(|_| IdentityParseError::InvalidUtf16)?;
    let document: IdentityDocument =
        serde_json::from_str(&json).map_err(|_| IdentityParseError::InvalidJson)?;
    let name_bytes = document.name.as_bytes();
    if name_bytes.contains(&0) {
        return Err(IdentityParseError::EmbeddedNul);
    }
    if name_bytes.len() >= IDENTITY_NAME_CAPACITY {
        return Err(IdentityParseError::NameTooLong);
    }

    let mut name = [0; IDENTITY_NAME_CAPACITY];
    name[..name_bytes.len()].copy_from_slice(name_bytes);

    Ok(MumbleIdentity {
        name,
        profession: MumbleProfession::from_raw(document.profession),
        specialization: document.spec,
        race: MumbleRace::from_raw(document.race),
        map_id: document.map_id,
        world_id: document.world_id,
        team_color_id: document.team_color_id,
        is_commander: u8::from(document.commander),
        fov: document.fov,
        ui_size: MumbleUiScale::from_raw(document.uisz),
    })
}

#[cfg(test)]
mod tests {
    use nexus_abi::{MumbleProfession, MumbleRace, MumbleUiScale};

    use super::{IdentityParseError, parse_identity};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn parses_the_legacy_identity_shape() {
        let identity = parse_identity(&wide(
            r#"{"name":"Example","profession":9,"spec":63,"race":2,"map_id":15,"world_id":1001,"team_color_id":4,"commander":true,"fov":1.2,"uisz":3}"#,
        ));
        assert!(identity.is_ok());
        let identity = identity.unwrap_or_default();
        assert_eq!(&identity.name[..8], b"Example\0");
        assert_eq!(identity.profession, MumbleProfession::REVENANT);
        assert_eq!(identity.specialization, 63);
        assert_eq!(identity.race, MumbleRace::HUMAN);
        assert_eq!(identity.map_id, 15);
        assert_eq!(identity.world_id, 1001);
        assert_eq!(identity.team_color_id, 4);
        assert_eq!(identity.is_commander, 1);
        assert_eq!(identity.ui_size, MumbleUiScale::LARGER);
    }

    #[test]
    fn preserves_unknown_open_values() {
        let identity = parse_identity(&wide(
            r#"{"name":"Future","profession":250,"spec":1,"race":251,"map_id":2,"world_id":3,"team_color_id":4,"commander":false,"fov":1.0,"uisz":252}"#,
        ))
        .unwrap_or_default();
        assert_eq!(identity.profession.value(), 250);
        assert_eq!(identity.race.value(), 251);
        assert_eq!(identity.ui_size.value(), 252);
    }

    #[test]
    fn rejects_overflow_and_malformed_documents_without_echoing_them() {
        let long_name = parse_identity(&wide(
            r#"{"name":"12345678901234567890","profession":1,"spec":2,"race":3,"map_id":4,"world_id":5,"team_color_id":6,"commander":false,"fov":1.0,"uisz":1}"#,
        ));
        assert_eq!(long_name, Err(IdentityParseError::NameTooLong));
        assert_eq!(parse_identity(&[]), Err(IdentityParseError::Empty));
        assert_eq!(
            parse_identity(&[0xD800, 0]),
            Err(IdentityParseError::InvalidUtf16)
        );
        assert_eq!(
            parse_identity(&wide("not-json")),
            Err(IdentityParseError::InvalidJson)
        );
    }
}
