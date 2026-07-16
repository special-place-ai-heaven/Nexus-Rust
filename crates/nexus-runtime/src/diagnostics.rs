use core::fmt;

use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;

pub(crate) fn report_proxy_failure(error: &impl fmt::Display) {
    let message = format!("Nexus Rust proxy failure: {error}\0");
    let wide = message.encode_utf16().collect::<Vec<_>>();

    // SAFETY: `wide` is NUL-terminated and remains alive for the duration of
    // the synchronous Windows API call.
    unsafe { OutputDebugStringW(wide.as_ptr()) };
}

pub(crate) fn report_proxy_panic() {
    const MESSAGE: &[u16] = &[
        b'N' as u16,
        b'e' as u16,
        b'x' as u16,
        b'u' as u16,
        b's' as u16,
        b' ' as u16,
        b'R' as u16,
        b'u' as u16,
        b's' as u16,
        b't' as u16,
        b' ' as u16,
        b'p' as u16,
        b'r' as u16,
        b'o' as u16,
        b'x' as u16,
        b'y' as u16,
        b' ' as u16,
        b'p' as u16,
        b'a' as u16,
        b'n' as u16,
        b'i' as u16,
        b'c' as u16,
        0,
    ];

    // SAFETY: `MESSAGE` is statically allocated and NUL-terminated.
    unsafe { OutputDebugStringW(MESSAGE.as_ptr()) };
}
