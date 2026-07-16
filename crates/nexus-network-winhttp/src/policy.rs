use std::fmt;
use std::time::Duration;

use nexus_network::{HttpRequest, TransportError};
use thiserror::Error;

const DEFAULT_RESOLVE_MILLIS: i32 = 15_000;
const DEFAULT_CONNECT_MILLIS: i32 = 15_000;
const DEFAULT_SEND_MILLIS: i32 = 30_000;
const DEFAULT_RECEIVE_MILLIS: i32 = 30_000;

/// Timeout operation named by [`WinHttpTimeoutsError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutStage {
    /// Host name resolution.
    Resolve,
    /// TCP/TLS connection establishment.
    Connect,
    /// Request transmission.
    Send,
    /// Response and body reads.
    Receive,
}

/// Invalid finite timeout configuration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WinHttpTimeoutsError {
    /// WinHTTP interprets zero as infinite, which this adapter forbids.
    #[error("{0:?} timeout must be finite and non-zero")]
    Zero(TimeoutStage),
    /// WinHTTP accepts signed 32-bit millisecond values only.
    #[error("{0:?} timeout exceeds the WinHTTP millisecond range")]
    TooLong(TimeoutStage),
}

/// Explicit finite resolve/connect/send/receive timeouts for WinHTTP.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct WinHttpTimeouts {
    resolve_millis: i32,
    connect_millis: i32,
    send_millis: i32,
    receive_millis: i32,
}

impl WinHttpTimeouts {
    /// Validates and constructs a timeout policy.
    ///
    /// Positive sub-millisecond durations round up to one millisecond. Zero is
    /// rejected because WinHTTP treats it as an infinite timeout.
    ///
    /// # Errors
    ///
    /// Returns the stage whose duration is zero or exceeds `i32::MAX`
    /// milliseconds.
    pub fn new(
        resolve: Duration,
        connect: Duration,
        send: Duration,
        receive: Duration,
    ) -> Result<Self, WinHttpTimeoutsError> {
        Ok(Self {
            resolve_millis: duration_millis(resolve, TimeoutStage::Resolve)?,
            connect_millis: duration_millis(connect, TimeoutStage::Connect)?,
            send_millis: duration_millis(send, TimeoutStage::Send)?,
            receive_millis: duration_millis(receive, TimeoutStage::Receive)?,
        })
    }

    /// Returns the name-resolution timeout.
    #[must_use]
    pub const fn resolve(self) -> Duration {
        Duration::from_millis(self.resolve_millis as u64)
    }

    /// Returns the connection timeout.
    #[must_use]
    pub const fn connect(self) -> Duration {
        Duration::from_millis(self.connect_millis as u64)
    }

    /// Returns the request-send timeout.
    #[must_use]
    pub const fn send(self) -> Duration {
        Duration::from_millis(self.send_millis as u64)
    }

    /// Returns the response-receive timeout.
    #[must_use]
    pub const fn receive(self) -> Duration {
        Duration::from_millis(self.receive_millis as u64)
    }

    pub(crate) const fn milliseconds(self) -> [i32; 4] {
        [
            self.resolve_millis,
            self.connect_millis,
            self.send_millis,
            self.receive_millis,
        ]
    }
}

impl Default for WinHttpTimeouts {
    fn default() -> Self {
        Self {
            resolve_millis: DEFAULT_RESOLVE_MILLIS,
            connect_millis: DEFAULT_CONNECT_MILLIS,
            send_millis: DEFAULT_SEND_MILLIS,
            receive_millis: DEFAULT_RECEIVE_MILLIS,
        }
    }
}

impl fmt::Debug for WinHttpTimeouts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WinHttpTimeouts")
            .field("resolve", &self.resolve())
            .field("connect", &self.connect())
            .field("send", &self.send())
            .field("receive", &self.receive())
            .finish()
    }
}

fn duration_millis(duration: Duration, stage: TimeoutStage) -> Result<i32, WinHttpTimeoutsError> {
    if duration.is_zero() {
        return Err(WinHttpTimeoutsError::Zero(stage));
    }
    let whole_millis = duration.as_millis();
    let rounded_millis = whole_millis.max(1);
    i32::try_from(rounded_millis).map_err(|_error| WinHttpTimeoutsError::TooLong(stage))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

pub(crate) struct ParsedRequest {
    pub(crate) scheme: Scheme,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) target: String,
}

impl ParsedRequest {
    pub(crate) fn from_request(request: &HttpRequest) -> Result<Self, TransportError> {
        parse_request_parts(request.base_url().as_str(), request.target())
    }
}

impl fmt::Debug for ParsedRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParsedRequest")
            .field("url", &"[redacted]")
            .finish()
    }
}

fn parse_request_parts(base_url: &str, target: &str) -> Result<ParsedRequest, TransportError> {
    let (scheme_text, authority) = base_url
        .split_once("://")
        .ok_or(TransportError::RequestFailed)?;
    let scheme = match scheme_text {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return Err(TransportError::RequestFailed),
    };
    let default_port = match scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };
    let (host, port) = parse_authority(authority, default_port)?;
    validate_target(target)?;
    Ok(ParsedRequest {
        scheme,
        host: host.to_owned(),
        port,
        target: target.to_owned(),
    })
}

fn parse_authority(authority: &str, default_port: u16) -> Result<(&str, u16), TransportError> {
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@', '\0'])
        || authority.chars().any(char::is_whitespace)
        || authority.chars().any(char::is_control)
    {
        return Err(TransportError::RequestFailed);
    }

    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed.find(']').ok_or(TransportError::RequestFailed)?;
        let host = &bracketed[..close];
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            let port_text = suffix
                .strip_prefix(':')
                .ok_or(TransportError::RequestFailed)?;
            parse_port(port_text)?
        };
        (host, port)
    } else {
        if authority.matches(':').count() > 1 {
            return Err(TransportError::RequestFailed);
        }
        match authority.rsplit_once(':') {
            Some((host, port_text)) => (host, parse_port(port_text)?),
            None => (authority, default_port),
        }
    };

    if host.is_empty() || host.contains(['/', '?', '#', '@', '\0', '[', ']']) {
        return Err(TransportError::RequestFailed);
    }
    Ok((host, port))
}

fn parse_port(value: &str) -> Result<u16, TransportError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TransportError::RequestFailed);
    }
    let port = value
        .parse::<u16>()
        .map_err(|_error| TransportError::RequestFailed)?;
    if port == 0 {
        return Err(TransportError::RequestFailed);
    }
    Ok(port)
}

fn validate_target(target: &str) -> Result<(), TransportError> {
    if !target.starts_with('/')
        || target.contains("://")
        || target.contains('#')
        || target.contains('\0')
        || target.chars().any(char::is_control)
    {
        return Err(TransportError::RequestFailed);
    }
    Ok(())
}

pub(crate) fn validate_status(status: u32) -> Result<u16, TransportError> {
    let status = u16::try_from(status).map_err(|_error| TransportError::InvalidResponse)?;
    if !(100..=599).contains(&status) {
        return Err(TransportError::InvalidResponse);
    }
    Ok(status)
}

pub(crate) fn parse_content_length_utf16(value: &[u16]) -> Result<u64, TransportError> {
    if value.is_empty() {
        return Err(TransportError::InvalidResponse);
    }
    value.iter().try_fold(0_u64, |current, unit| {
        let digit = match *unit {
            unit if (u16::from(b'0')..=u16::from(b'9')).contains(&unit) => {
                u64::from(unit - u16::from(b'0'))
            }
            _ => return Err(TransportError::InvalidResponse),
        };
        current
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            .ok_or(TransportError::InvalidResponse)
    })
}

pub(crate) struct BoundedBody {
    bytes: Vec<u8>,
    max_bytes: usize,
    declared_length: Option<u64>,
}

impl BoundedBody {
    pub(crate) fn new(
        max_bytes: usize,
        declared_length: Option<u64>,
    ) -> Result<Self, TransportError> {
        let max_as_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        if declared_length.is_some_and(|length| length > max_as_u64) {
            return Err(TransportError::BodyTooLarge);
        }

        let reserve = declared_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(max_bytes);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(reserve)
            .map_err(|_error| TransportError::RequestFailed)?;
        Ok(Self {
            bytes,
            max_bytes,
            declared_length,
        })
    }

    pub(crate) fn next_read_size(&self, buffer_size: usize) -> usize {
        self.max_bytes
            .saturating_sub(self.bytes.len())
            .saturating_add(1)
            .min(buffer_size)
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), TransportError> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if chunk.len() > remaining {
            return Err(TransportError::BodyTooLarge);
        }
        self.bytes
            .try_reserve(chunk.len())
            .map_err(|_error| TransportError::RequestFailed)?;
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>, TransportError> {
        let actual = match u64::try_from(self.bytes.len()) {
            Ok(value) => value,
            Err(_error) => return Err(TransportError::InvalidResponse),
        };
        if self
            .declared_length
            .is_some_and(|declared| declared != actual)
        {
            return Err(TransportError::InvalidResponse);
        }
        Ok(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_policy_is_finite_bounded_and_rounds_up() {
        let zero = WinHttpTimeouts::new(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert_eq!(zero, Err(WinHttpTimeoutsError::Zero(TimeoutStage::Resolve)));

        let too_long = WinHttpTimeouts::new(
            Duration::from_secs(1),
            Duration::from_millis(i32::MAX as u64 + 1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert_eq!(
            too_long,
            Err(WinHttpTimeoutsError::TooLong(TimeoutStage::Connect))
        );

        let Ok(rounded) = WinHttpTimeouts::new(
            Duration::from_nanos(1),
            Duration::from_secs(2),
            Duration::from_secs(3),
            Duration::from_secs(4),
        ) else {
            panic!("positive finite timeout policy should validate");
        };
        assert_eq!(rounded.resolve(), Duration::from_millis(1));
        assert_eq!(rounded.connect(), Duration::from_secs(2));
    }

    #[test]
    fn request_parts_cover_http_https_ports_ipv6_and_unicode() {
        let Ok(http) = parse_request_parts("http://example.test", "/path?q=1") else {
            panic!("HTTP origin should parse");
        };
        assert_eq!(http.scheme, Scheme::Http);
        assert_eq!(http.port, 80);

        let Ok(https) = parse_request_parts("https://example.test:8443", "/føø") else {
            panic!("HTTPS origin should parse");
        };
        assert_eq!(https.scheme, Scheme::Https);
        assert_eq!(https.host, "example.test");
        assert_eq!(https.port, 8443);
        assert_eq!(https.target, "/føø");

        let Ok(ipv6) = parse_request_parts("https://[2001:db8::1]", "/") else {
            panic!("bracketed IPv6 origin should parse");
        };
        assert_eq!(ipv6.host, "2001:db8::1");
        assert_eq!(ipv6.port, 443);

        let Ok(unicode) = parse_request_parts("https://bücher.example", "/") else {
            panic!("Unicode host should reach the IDN conversion layer");
        };
        assert_eq!(unicode.host, "bücher.example");
    }

    #[test]
    fn request_policy_rejects_ambiguous_or_truncated_urls() {
        for (base, target) in [
            ("https://example.test:0", "/"),
            ("https://example.test:", "/"),
            ("https://user@example.test", "/"),
            ("https://example.test", "/safe\0hidden"),
            ("https://example.test", "/safe\r\nInjected: value"),
            ("https://2001:db8::1", "/"),
        ] {
            assert_eq!(
                parse_request_parts(base, target).map(|_parsed| ()),
                Err(TransportError::RequestFailed)
            );
        }
    }

    #[test]
    fn parsed_request_debug_redacts_host_and_target() {
        let Ok(parsed) = parse_request_parts("https://private.example", "/?token=private") else {
            panic!("test request should parse");
        };
        let rendered = format!("{parsed:?}");
        assert_eq!(rendered, "ParsedRequest { url: \"[redacted]\" }");
    }

    #[test]
    fn status_and_content_length_parsing_is_strict() {
        assert_eq!(validate_status(200), Ok(200));
        assert_eq!(validate_status(99), Err(TransportError::InvalidResponse));
        assert_eq!(validate_status(600), Err(TransportError::InvalidResponse));

        let valid: Vec<u16> = "18446744073709551615".encode_utf16().collect();
        assert_eq!(parse_content_length_utf16(&valid), Ok(u64::MAX));
        for invalid in ["", " 12", "+12", "12, 12", "18446744073709551616"] {
            let wide: Vec<u16> = invalid.encode_utf16().collect();
            assert_eq!(
                parse_content_length_utf16(&wide),
                Err(TransportError::InvalidResponse)
            );
        }
    }

    #[test]
    fn bounded_body_never_retains_bytes_past_the_limit() {
        assert!(matches!(
            BoundedBody::new(3, Some(4)),
            Err(TransportError::BodyTooLarge)
        ));

        let Ok(mut body) = BoundedBody::new(3, None) else {
            panic!("small body accumulator should initialize");
        };
        assert_eq!(body.next_read_size(8 * 1024), 4);
        assert_eq!(body.push(b"abc"), Ok(()));
        assert_eq!(body.next_read_size(8 * 1024), 1);
        assert_eq!(body.push(b"d"), Err(TransportError::BodyTooLarge));
        assert_eq!(body.finish(), Ok(b"abc".to_vec()));

        let Ok(mut mismatch) = BoundedBody::new(8, Some(4)) else {
            panic!("declared body should initialize");
        };
        assert_eq!(mismatch.push(b"abc"), Ok(()));
        assert_eq!(mismatch.finish(), Err(TransportError::InvalidResponse));
    }
}
