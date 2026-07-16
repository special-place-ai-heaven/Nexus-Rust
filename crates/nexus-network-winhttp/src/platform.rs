use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{NonNull, null, null_mut};

use nexus_network::{HttpRequest, TransportError, TransportResponse};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::Globalization::{IDN_USE_STD3_ASCII_RULES, IdnToAscii};
use windows_sys::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_HEADER_NOT_FOUND, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};

use crate::policy::{
    BoundedBody, ParsedRequest, Scheme, parse_content_length_utf16, validate_status,
};
use crate::{InitializationStage, WinHttpTimeouts};

const USER_AGENT: [u16; 10] = [
    b'N' as u16,
    b'e' as u16,
    b'x' as u16,
    b'u' as u16,
    b's' as u16,
    b'/' as u16,
    b'1' as u16,
    b'.' as u16,
    b'0' as u16,
    0,
];
const GET: [u16; 4] = [b'G' as u16, b'E' as u16, b'T' as u16, 0];
const READ_BUFFER_BYTES: usize = 16 * 1024;
const CONTENT_LENGTH_UNITS: usize = 32;

pub(crate) struct InitFailure {
    pub(crate) stage: InitializationStage,
    pub(crate) code: u32,
}

struct InternetHandle(NonNull<c_void>);

impl InternetHandle {
    fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    fn as_raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper is the unique owner of a non-null HINTERNET
        // returned by WinHTTP. The synchronous session installs no callbacks,
        // and Drop calls close exactly once.
        let _closed = unsafe { WinHttpCloseHandle(self.as_raw()) };
    }
}

pub(crate) struct Session {
    handle: InternetHandle,
}

impl Session {
    pub(crate) fn open(timeouts: WinHttpTimeouts) -> Result<Self, InitFailure> {
        // SAFETY: all string pointers are valid null-terminated UTF-16 or null,
        // and flags request a synchronous automatic-proxy session.
        let raw = unsafe {
            WinHttpOpen(
                USER_AGENT.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            )
        };
        let handle = InternetHandle::from_raw(raw).ok_or_else(|| InitFailure {
            stage: InitializationStage::OpenSession,
            // SAFETY: called immediately after the failed WinHttpOpen call on
            // the same thread.
            code: unsafe { GetLastError() },
        })?;
        let [resolve, connect, send, receive] = timeouts.milliseconds();
        // SAFETY: the handle is a live session handle and each timeout is a
        // validated positive signed-millisecond value.
        let configured =
            unsafe { WinHttpSetTimeouts(handle.as_raw(), resolve, connect, send, receive) };
        if configured == 0 {
            // SAFETY: called immediately after the failed WinHttpSetTimeouts
            // call on the same thread.
            let code = unsafe { GetLastError() };
            return Err(InitFailure {
                stage: InitializationStage::ConfigureTimeouts,
                code,
            });
        }
        Ok(Self { handle })
    }
}

pub(crate) fn execute(
    session: &Session,
    request: &HttpRequest,
    max_body_bytes: usize,
) -> Result<TransportResponse, TransportError> {
    let parsed = ParsedRequest::from_request(request)?;
    let host = encode_host(&parsed.host)?;
    let target = wide_null(&parsed.target)?;

    // SAFETY: session is live, host is null-terminated UTF-16, the port was
    // validated, and the reserved argument is zero.
    let connect_raw =
        unsafe { WinHttpConnect(session.handle.as_raw(), host.as_ptr(), parsed.port, 0) };
    let connect = InternetHandle::from_raw(connect_raw).ok_or(TransportError::RequestFailed)?;

    let flags = match parsed.scheme {
        Scheme::Http => 0,
        Scheme::Https => WINHTTP_FLAG_SECURE,
    };
    // SAFETY: the connection is live; verb and target are null-terminated
    // UTF-16; optional version, referrer, and accept-type pointers are null.
    let request_raw = unsafe {
        WinHttpOpenRequest(
            connect.as_raw(),
            GET.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            null(),
            flags,
        )
    };
    let request_handle =
        InternetHandle::from_raw(request_raw).ok_or(TransportError::RequestFailed)?;

    // SAFETY: the request handle is live and this GET supplies neither custom
    // headers nor request body.
    let sent = unsafe { WinHttpSendRequest(request_handle.as_raw(), null(), 0, null(), 0, 0, 0) };
    if sent == 0 {
        return Err(TransportError::RequestFailed);
    }
    // SAFETY: the request was sent synchronously and the reserved pointer is
    // null as required.
    let received = unsafe { WinHttpReceiveResponse(request_handle.as_raw(), null_mut()) };
    if received == 0 {
        return Err(TransportError::RequestFailed);
    }

    let status_code = query_status(&request_handle)?;
    let content_length = query_content_length(&request_handle)?;
    let mut body = BoundedBody::new(max_body_bytes, content_length)?;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];

    loop {
        let request_bytes = body.next_read_size(buffer.len());
        let request_bytes =
            u32::try_from(request_bytes).map_err(|_error| TransportError::InvalidResponse)?;
        let mut read = 0_u32;
        // SAFETY: the request handle has a received response, the buffer is
        // writable for request_bytes bytes, and read points to a live u32.
        let read_ok = unsafe {
            WinHttpReadData(
                request_handle.as_raw(),
                buffer.as_mut_ptr().cast(),
                request_bytes,
                &mut read,
            )
        };
        if read_ok == 0 {
            return Err(TransportError::RequestFailed);
        }
        if read == 0 {
            break;
        }
        let read = usize::try_from(read).map_err(|_error| TransportError::InvalidResponse)?;
        if read > buffer.len() || read > request_bytes as usize {
            return Err(TransportError::InvalidResponse);
        }
        body.push(&buffer[..read])?;
    }

    let body = body.finish()?;
    Ok(TransportResponse::new(status_code, body, content_length))
}

fn query_status(request: &InternetHandle) -> Result<u16, TransportError> {
    let mut status = 0_u32;
    let mut bytes =
        u32::try_from(size_of::<u32>()).map_err(|_error| TransportError::InvalidResponse)?;
    // SAFETY: request has a received response, status is writable for bytes,
    // and null selects the known status header with no enumeration index.
    let queried = unsafe {
        WinHttpQueryHeaders(
            request.as_raw(),
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut bytes,
            null_mut(),
        )
    };
    if queried == 0 || bytes as usize != size_of::<u32>() {
        return Err(TransportError::InvalidResponse);
    }
    validate_status(status)
}

fn query_content_length(request: &InternetHandle) -> Result<Option<u64>, TransportError> {
    let mut buffer = [0_u16; CONTENT_LENGTH_UNITS];
    let mut bytes = u32::try_from(size_of::<[u16; CONTENT_LENGTH_UNITS]>())
        .map_err(|_error| TransportError::InvalidResponse)?;
    // SAFETY: request has a received response; buffer and byte count describe
    // valid writable storage; null selects the known Content-Length header.
    let queried = unsafe {
        WinHttpQueryHeaders(
            request.as_raw(),
            WINHTTP_QUERY_CONTENT_LENGTH,
            null(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
        )
    };
    if queried == 0 {
        // SAFETY: called immediately after WinHttpQueryHeaders failed on this
        // thread, before any other Windows API call.
        let code = unsafe { GetLastError() };
        return match code {
            ERROR_WINHTTP_HEADER_NOT_FOUND => Ok(None),
            ERROR_INSUFFICIENT_BUFFER => Err(TransportError::InvalidResponse),
            _ => Err(TransportError::RequestFailed),
        };
    }

    let bytes = usize::try_from(bytes).map_err(|_error| TransportError::InvalidResponse)?;
    if bytes == 0 || bytes % size_of::<u16>() != 0 || bytes > size_of_val(&buffer) {
        return Err(TransportError::InvalidResponse);
    }
    let units = bytes / size_of::<u16>();
    parse_content_length_utf16(&buffer[..units]).map(Some)
}

fn encode_host(host: &str) -> Result<Vec<u16>, TransportError> {
    if host.is_ascii() {
        return wide_null(host);
    }

    let input = wide_without_null(host)?;
    let input_length =
        i32::try_from(input.len()).map_err(|_error| TransportError::RequestFailed)?;
    // SAFETY: input is valid UTF-16 encoded from Rust UTF-8; a null output and
    // zero capacity request the required output size.
    let required = unsafe {
        IdnToAscii(
            IDN_USE_STD3_ASCII_RULES,
            input.as_ptr(),
            input_length,
            null_mut(),
            0,
        )
    };
    if required <= 0 {
        return Err(TransportError::RequestFailed);
    }
    let required = usize::try_from(required).map_err(|_error| TransportError::RequestFailed)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(required.saturating_add(1))
        .map_err(|_error| TransportError::RequestFailed)?;
    output.resize(required, 0);
    // SAFETY: output is writable for required UTF-16 units and input remains
    // live for the duration of the conversion.
    let written = unsafe {
        IdnToAscii(
            IDN_USE_STD3_ASCII_RULES,
            input.as_ptr(),
            input_length,
            output.as_mut_ptr(),
            i32::try_from(required).map_err(|_error| TransportError::RequestFailed)?,
        )
    };
    if usize::try_from(written).ok() != Some(required) {
        return Err(TransportError::RequestFailed);
    }
    output.push(0);
    Ok(output)
}

fn wide_null(value: &str) -> Result<Vec<u16>, TransportError> {
    if value.contains('\0') {
        return Err(TransportError::RequestFailed);
    }
    let mut wide = wide_without_null(value)?;
    wide.try_reserve_exact(1)
        .map_err(|_error| TransportError::RequestFailed)?;
    wide.push(0);
    Ok(wide)
}

fn wide_without_null(value: &str) -> Result<Vec<u16>, TransportError> {
    let mut wide = Vec::new();
    wide.try_reserve_exact(value.len())
        .map_err(|_error| TransportError::RequestFailed)?;
    wide.extend(value.encode_utf16());
    Ok(wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_conversion_preserves_unicode_and_terminates_once() {
        let Ok(wide) = wide_null("/føø/😀") else {
            panic!("valid Rust text should encode as UTF-16");
        };
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(wide.iter().filter(|unit| **unit == 0).count(), 1);
        let Ok(decoded) = String::from_utf16(&wide[..wide.len() - 1]) else {
            panic!("adapter-produced UTF-16 should decode");
        };
        assert_eq!(decoded, "/føø/😀");
    }

    #[test]
    fn unicode_host_is_converted_to_ascii_idn_without_network_access() {
        let Ok(wide) = encode_host("bücher.example") else {
            panic!("valid IDN should convert to Punycode");
        };
        assert_eq!(wide.last(), Some(&0));
        let Ok(decoded) = String::from_utf16(&wide[..wide.len() - 1]) else {
            panic!("IDN output should be valid UTF-16");
        };
        assert_eq!(decoded, "xn--bcher-kva.example");
    }
}
