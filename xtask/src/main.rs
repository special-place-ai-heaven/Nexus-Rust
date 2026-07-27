//! Repository verification tasks that need to inspect built artifacts.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

const EXPECTED_EXPORTS: [(&str, u32); 20] = [
    ("D3D11CoreCreateDevice", 1),
    ("D3D11CoreCreateLayeredDevice", 2),
    ("D3D11CoreGetLayeredDeviceSize", 3),
    ("D3D11CoreRegisterLayers", 4),
    ("D3D11CreateDevice", 5),
    ("D3D11CreateDeviceAndSwapChain", 6),
    ("Direct3DCreate9", 7),
    ("Direct3DCreate9Ex", 8),
    ("D3DPERF_BeginEvent", 9),
    ("D3DPERF_EndEvent", 10),
    ("D3DPERF_SetMarker", 11),
    ("D3DPERF_SetRegion", 12),
    ("D3DPERF_QueryRepeatFrame", 13),
    ("D3DPERF_SetOptions", 14),
    ("D3DPERF_GetStatus", 15),
    ("CreateDXGIFactory", 16),
    ("CreateDXGIFactory1", 17),
    ("CreateDXGIFactory2", 18),
    ("DXGIGetDebugInterface1", 19),
    ("DXGIDeclareAdapterRemovalSupport", 20),
];

/// Lowercase module-name prefixes identifying a dynamically linked C/C++ runtime.
///
/// `VCRUNTIME140.dll` and `MSVCP140.dll` ship only with the Visual C++
/// redistributable, so importing them makes the proxy unloadable on a machine that
/// has never installed it — and because Guild Wars 2 imports `d3d11.dll` statically,
/// an unloadable proxy means the game does not start at all. The `api-ms-win-crt-*`
/// forwarders and `ucrtbase.dll` are part of Windows 10 and resolve there, but a
/// static-CRT build emits none of these: any hit proves the `crt-static` rustflags in
/// `.cargo/config.toml` did not reach this build.
const DYNAMIC_RUNTIME_PREFIXES: [&str; 5] =
    ["api-ms-win-crt-", "msvcp", "msvcr", "ucrtbase", "vcruntime"];

/// Lowercase module names the release proxy is permitted to import.
///
/// Every entry is either OS-guaranteed or a module the C++ reference links too
/// (`d3dcompiler_47` and `xinput1_4` arrive through Dear ImGui's own pragmas). Adding
/// a name is a decision about the dependency surface and the supported Windows floor
/// — `bcryptprimitives.dll` puts the floor at Windows 10 — so an unlisted import
/// fails the gate instead of being accepted silently.
const ALLOWED_IMPORTS: [&str; 10] = [
    "bcryptprimitives.dll",
    "d3dcompiler_47.dll",
    "kernel32.dll",
    "normaliz.dll",
    "ntdll.dll",
    "oleaut32.dll",
    "shell32.dll",
    "user32.dll",
    "winhttp.dll",
    "xinput1_4.dll",
];

/// Lowercase prefix for Windows API sets, which are OS components rather than
/// redistributables.
///
/// Allowed by prefix because the exact api-set names a build binds vary between
/// toolchain versions; pinning them individually would make this gate fail on
/// unrelated upgrades. `api-ms-win-crt-*` is excluded by
/// [`DYNAMIC_RUNTIME_PREFIXES`], which is checked first.
const ALLOWED_IMPORT_PREFIX: &str = "api-ms-win-core-";

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let mut arguments = env::args_os().skip(1);
    let Some(command) = arguments.next() else {
        return Err(
            "expected `verify-exports [path]`, `verify-imports [path]`, or `smoke-proxy [path]`"
                .into(),
        );
    };

    let path = arguments
        .next()
        .map_or_else(|| PathBuf::from("target/debug/d3d11.dll"), PathBuf::from);
    if arguments.next().is_some() {
        return Err(format!(
            "{} accepts at most one DLL path",
            command.to_string_lossy()
        ));
    }

    if command == OsStr::new("verify-exports") {
        verify_exports(&path)?;
        Ok(format!(
            "verified {} named exports and ordinals in {}",
            EXPECTED_EXPORTS.len(),
            path.display()
        ))
    } else if command == OsStr::new("verify-imports") {
        let modules = verify_imports(&path)?;
        Ok(format!(
            "verified {} imported modules in {}; the C runtime is linked statically",
            modules.len(),
            path.display()
        ))
    } else if command == OsStr::new("smoke-proxy") {
        smoke_proxy(&path)
    } else {
        Err(format!("unknown task `{}`", command.to_string_lossy()))
    }
}

#[cfg(windows)]
fn smoke_proxy(path: &Path) -> Result<String, String> {
    use std::{mem::transmute_copy, os::windows::ffi::OsStrExt};

    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let absolute = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    let wide = absolute
        .as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: `wide` is NUL-terminated and alive for the duration of the call.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(format!(
            "failed to load {}: {}",
            absolute.display(),
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: `module` is live and the export name is static and NUL-terminated.
    let address = unsafe { GetProcAddress(module, c"D3DPERF_GetStatus".as_ptr().cast()) }
        .ok_or_else(|| "D3DPERF_GetStatus was not resolvable".to_owned())?;
    type GetStatus = unsafe extern "system" fn() -> u32;
    // SAFETY: the export's Windows SDK signature is `DWORD WINAPI()` and both
    // source and destination are pointer-sized function pointers.
    let get_status = unsafe { transmute_copy::<_, GetStatus>(&address) };
    // SAFETY: the function pointer came from the exact named export above.
    let status = unsafe { get_status() };

    Ok(format!(
        "loaded {} and forwarded D3DPERF_GetStatus successfully (status {status})",
        absolute.display()
    ))
}

#[cfg(not(windows))]
fn smoke_proxy(_path: &Path) -> Result<String, String> {
    Err("proxy smoke testing requires Windows".into())
}

fn verify_exports(path: &Path) -> Result<(), String> {
    let image =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let actual = read_pe_exports(&image)?;
    let expected = EXPECTED_EXPORTS
        .iter()
        .map(|(name, ordinal)| (*ordinal, (*name).to_owned()))
        .collect::<BTreeMap<_, _>>();

    if actual != expected {
        return Err(format!(
            "export table mismatch\nexpected: {expected:#?}\nactual: {actual:#?}"
        ));
    }
    Ok(())
}

fn verify_imports(path: &Path) -> Result<Vec<String>, String> {
    let image =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let modules = read_pe_imports(&image)?;

    let mut dynamic_runtime = Vec::new();
    let mut unexpected = Vec::new();
    for module in &modules {
        let lowered = module.to_ascii_lowercase();
        if DYNAMIC_RUNTIME_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
        {
            dynamic_runtime.push(module.clone());
        } else if !lowered.starts_with(ALLOWED_IMPORT_PREFIX)
            && !ALLOWED_IMPORTS.contains(&lowered.as_str())
        {
            unexpected.push(module.clone());
        }
    }

    if !dynamic_runtime.is_empty() {
        return Err(format!(
            "the proxy links the C runtime dynamically: {dynamic_runtime:?}\n\
             a static-CRT build imports none of these, so the `crt-static` rustflags in \
             .cargo/config.toml did not reach this build\n\
             all imports: {modules:?}"
        ));
    }
    if !unexpected.is_empty() {
        return Err(format!(
            "unexpected imports: {unexpected:?}\n\
             add each to ALLOWED_IMPORTS only as a deliberate decision about the dependency \
             surface and the supported Windows floor\n\
             all imports: {modules:?}"
        ));
    }
    Ok(modules)
}

fn read_pe_imports(image: &[u8]) -> Result<Vec<String>, String> {
    let headers = read_pe_headers(image)?;
    if read_u32(image, checked_add(headers.optional, 108)?)? < 2 {
        return Err("PE image has no import data directory".into());
    }

    let import_rva = read_u32(image, checked_add(headers.optional, 120)?)?;
    if import_rva == 0 {
        return Err("PE image has an empty import data directory".into());
    }

    let mut descriptor = rva_to_offset(import_rva, headers.size_of_headers, &headers.sections)?;
    let mut modules = Vec::new();
    loop {
        // A descriptor whose name RVA is zero terminates the table.
        let name_rva = read_u32(image, checked_add(descriptor, 12)?)?;
        if name_rva == 0 {
            break;
        }
        let name_offset = rva_to_offset(name_rva, headers.size_of_headers, &headers.sections)?;
        modules.push(read_ascii_z(image, name_offset)?);
        if modules.len() > 512 {
            return Err("PE import descriptor table is unreasonably long".into());
        }
        descriptor = checked_add(descriptor, 20)?;
    }

    // A PE may name the same module in more than one descriptor, and casing is not
    // normalised by the linker.
    modules.sort_by_key(|module| module.to_ascii_lowercase());
    modules.dedup_by_key(|module| module.to_ascii_lowercase());
    Ok(modules)
}

struct PeHeaders {
    optional: usize,
    size_of_headers: u32,
    sections: Vec<Section>,
}

fn read_pe_headers(image: &[u8]) -> Result<PeHeaders, String> {
    if image.get(0..2) != Some(b"MZ") {
        return Err("artifact has no DOS MZ header".into());
    }

    let pe_offset = usize_from_u32(read_u32(image, 0x3c)?)?;
    if image.get(pe_offset..pe_offset.saturating_add(4)) != Some(b"PE\0\0") {
        return Err("artifact has no PE signature".into());
    }
    if read_u16(image, checked_add(pe_offset, 4)?)? != 0x8664 {
        return Err("artifact is not AMD64".into());
    }

    let section_count = usize::from(read_u16(image, checked_add(pe_offset, 6)?)?);
    let optional_size = usize::from(read_u16(image, checked_add(pe_offset, 20)?)?);
    let optional = checked_add(pe_offset, 24)?;
    if read_u16(image, optional)? != 0x20b {
        return Err("artifact is not PE32+".into());
    }

    let size_of_headers = read_u32(image, checked_add(optional, 60)?)?;
    let section_table = checked_add(optional, optional_size)?;
    let sections = read_sections(image, section_table, section_count)?;
    Ok(PeHeaders {
        optional,
        size_of_headers,
        sections,
    })
}

fn read_pe_exports(image: &[u8]) -> Result<BTreeMap<u32, String>, String> {
    let PeHeaders {
        optional,
        size_of_headers,
        sections,
    } = read_pe_headers(image)?;
    if read_u32(image, checked_add(optional, 108)?)? < 1 {
        return Err("PE image has no export data directory".into());
    }

    let export_rva = read_u32(image, checked_add(optional, 112)?)?;
    let export_size = read_u32(image, checked_add(optional, 116)?)?;
    if export_rva == 0 || export_size == 0 {
        return Err("PE image has an empty export data directory".into());
    }

    let export_offset = rva_to_offset(export_rva, size_of_headers, &sections)?;
    let ordinal_base = read_u32(image, checked_add(export_offset, 16)?)?;
    let function_count = read_u32(image, checked_add(export_offset, 20)?)?;
    let name_count = read_u32(image, checked_add(export_offset, 24)?)?;
    let function_rva = read_u32(image, checked_add(export_offset, 28)?)?;
    let name_rva = read_u32(image, checked_add(export_offset, 32)?)?;
    let ordinal_rva = read_u32(image, checked_add(export_offset, 36)?)?;

    let expected_count = u32::try_from(EXPECTED_EXPORTS.len())
        .map_err(|_| "expected export count did not fit u32".to_owned())?;
    if function_count != expected_count || name_count != expected_count {
        return Err(format!(
            "expected {expected_count} named functions, found {function_count} functions and {name_count} names"
        ));
    }

    let functions = rva_to_offset(function_rva, size_of_headers, &sections)?;
    let names = rva_to_offset(name_rva, size_of_headers, &sections)?;
    let ordinals = rva_to_offset(ordinal_rva, size_of_headers, &sections)?;
    let export_end = export_rva
        .checked_add(export_size)
        .ok_or_else(|| "export RVA range overflowed".to_owned())?;
    let mut result = BTreeMap::new();

    for index in 0..usize_from_u32(name_count)? {
        let name_entry = checked_add(names, checked_mul(index, 4)?)?;
        let ordinal_entry = checked_add(ordinals, checked_mul(index, 2)?)?;
        let current_name_rva = read_u32(image, name_entry)?;
        let function_index = u32::from(read_u16(image, ordinal_entry)?);
        if function_index >= function_count {
            return Err("export name referenced an out-of-range function".into());
        }

        let function_entry =
            checked_add(functions, checked_mul(usize_from_u32(function_index)?, 4)?)?;
        let target_rva = read_u32(image, function_entry)?;
        if target_rva == 0 {
            return Err("export referenced an empty function slot".into());
        }
        if (export_rva..export_end).contains(&target_rva) {
            return Err("export forwarders are not allowed in the proxy surface".into());
        }

        let current_name_offset = rva_to_offset(current_name_rva, size_of_headers, &sections)?;
        let current_name = read_ascii_z(image, current_name_offset)?;
        let ordinal = ordinal_base
            .checked_add(function_index)
            .ok_or_else(|| "export ordinal overflowed".to_owned())?;
        if result.insert(ordinal, current_name).is_some() {
            return Err(format!("duplicate export ordinal {ordinal}"));
        }
    }

    Ok(result)
}

#[derive(Clone, Copy)]
struct Section {
    virtual_size: u32,
    virtual_address: u32,
    raw_size: u32,
    raw_offset: u32,
}

fn read_sections(image: &[u8], start: usize, count: usize) -> Result<Vec<Section>, String> {
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let section = checked_add(start, checked_mul(index, 40)?)?;
        sections.push(Section {
            virtual_size: read_u32(image, checked_add(section, 8)?)?,
            virtual_address: read_u32(image, checked_add(section, 12)?)?,
            raw_size: read_u32(image, checked_add(section, 16)?)?,
            raw_offset: read_u32(image, checked_add(section, 20)?)?,
        });
    }
    Ok(sections)
}

fn rva_to_offset(rva: u32, size_of_headers: u32, sections: &[Section]) -> Result<usize, String> {
    if rva < size_of_headers {
        return usize_from_u32(rva);
    }

    for section in sections {
        let span = section.virtual_size.max(section.raw_size);
        let Some(end) = section.virtual_address.checked_add(span) else {
            continue;
        };
        if (section.virtual_address..end).contains(&rva) {
            let within = rva - section.virtual_address;
            if within >= section.raw_size {
                return Err(format!("RVA 0x{rva:x} points outside section file data"));
            }
            let offset = section
                .raw_offset
                .checked_add(within)
                .ok_or_else(|| "section file offset overflowed".to_owned())?;
            return usize_from_u32(offset);
        }
    }

    Err(format!("RVA 0x{rva:x} does not map to a PE section"))
}

fn read_ascii_z(image: &[u8], start: usize) -> Result<String, String> {
    let tail = image
        .get(start..)
        .ok_or_else(|| "string offset is outside the PE image".to_owned())?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "unterminated export name".to_owned())?;
    let bytes = &tail[..length];
    if !bytes.is_ascii() {
        return Err("export name is not ASCII".into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|error| format!("invalid export name: {error}"))
}

fn read_u16(image: &[u8], offset: usize) -> Result<u16, String> {
    let bytes = image
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| format!("truncated PE field at file offset 0x{offset:x}"))?;
    let array = <[u8; 2]>::try_from(bytes)
        .map_err(|_| format!("invalid 16-bit PE field at file offset 0x{offset:x}"))?;
    Ok(u16::from_le_bytes(array))
}

fn read_u32(image: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = image
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| format!("truncated PE field at file offset 0x{offset:x}"))?;
    let array = <[u8; 4]>::try_from(bytes)
        .map_err(|_| format!("invalid 32-bit PE field at file offset 0x{offset:x}"))?;
    Ok(u32::from_le_bytes(array))
}

fn usize_from_u32(value: u32) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("value {value} did not fit usize"))
}

fn checked_add(left: usize, right: usize) -> Result<usize, String> {
    left.checked_add(right)
        .ok_or_else(|| "PE file offset overflowed".to_owned())
}

fn checked_mul(left: usize, right: usize) -> Result<usize, String> {
    left.checked_mul(right)
        .ok_or_else(|| "PE table size overflowed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOWED_IMPORT_PREFIX, ALLOWED_IMPORTS, DYNAMIC_RUNTIME_PREFIXES, read_pe_exports,
        read_pe_imports,
    };

    /// Mirrors the classification in `verify_imports` so the policy can be checked
    /// without a PE image.
    fn is_dynamic_runtime(module: &str) -> bool {
        let lowered = module.to_ascii_lowercase();
        DYNAMIC_RUNTIME_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
    }

    fn is_allowed(module: &str) -> bool {
        let lowered = module.to_ascii_lowercase();
        lowered.starts_with(ALLOWED_IMPORT_PREFIX) || ALLOWED_IMPORTS.contains(&lowered.as_str())
    }

    #[test]
    fn rejects_non_pe_input() {
        assert!(read_pe_exports(b"not a PE image").is_err());
        assert!(read_pe_imports(b"not a PE image").is_err());
    }

    #[test]
    fn classifies_the_redistributable_runtime_as_dynamic_regardless_of_case() {
        for module in [
            "VCRUNTIME140.dll",
            "vcruntime140d.dll",
            "MSVCP140.dll",
            "msvcr120.dll",
            "ucrtbase.dll",
            "api-ms-win-crt-heap-l1-1-0.dll",
            "API-MS-WIN-CRT-STDIO-L1-1-0.DLL",
        ] {
            assert!(is_dynamic_runtime(module), "{module} must be rejected");
        }
    }

    #[test]
    fn separates_os_api_sets_from_the_crt_api_sets() {
        // Both are `api-ms-win-*`; only the CRT family indicates dynamic linkage.
        assert!(is_allowed("api-ms-win-core-synch-l1-2-0.dll"));
        assert!(!is_dynamic_runtime("api-ms-win-core-synch-l1-2-0.dll"));
        assert!(is_dynamic_runtime("api-ms-win-crt-runtime-l1-1-0.dll"));
        assert!(!is_allowed("api-ms-win-crt-runtime-l1-1-0.dll"));
    }

    #[test]
    fn admits_the_expected_modules_and_refuses_unlisted_ones() {
        assert!(is_allowed("KERNEL32.dll"), "casing must not matter");
        assert!(
            is_allowed("d3dcompiler_47.dll"),
            "the reference links it too"
        );
        assert!(
            !is_allowed("wininet.dll"),
            "an unlisted import is a decision"
        );
        assert!(!is_allowed("msvcp140.dll"));
    }
}
