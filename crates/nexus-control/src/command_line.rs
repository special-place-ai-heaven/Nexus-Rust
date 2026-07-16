use std::ffi::{OsStr, OsString};
use std::fmt;

const REDACTED: &str = "<redacted>";
const DEFAULT_MUMBLE_NAME: &str = "MumbleLink";

/// An owned command-line value whose normal formatting is always redacted.
///
/// Call [`Self::expose`] only at the boundary that must consume the value. Do
/// not pass the exposed value to a logger or diagnostic event.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct RedactedArg(OsString);

impl RedactedArg {
    /// Wraps an operating-system string without normalizing it.
    #[must_use]
    pub fn new(value: impl Into<OsString>) -> Self {
        Self(value.into())
    }

    /// Explicitly exposes the value to the subsystem that consumes it.
    #[must_use]
    pub fn expose(&self) -> &OsStr {
        &self.0
    }

    /// Consumes the wrapper and returns the original operating-system string.
    #[must_use]
    pub fn into_inner(self) -> OsString {
        self.0
    }
}

impl fmt::Debug for RedactedArg {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl fmt::Display for RedactedArg {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// Selects how Nexus discovers and intercepts swap-chain calls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookMode {
    /// Prefer per-object hooks and select the guarded fallback when required.
    #[default]
    Auto,
    /// Use only typed, per-object COM vtable hooks.
    Object,
    /// Use the process-global implementation hook as an explicit fallback.
    GlobalFallback,
    /// Observe swap-chain activity without rendering or mutating graphics state.
    Observe,
    /// Install no graphics hooks.
    Off,
}

impl HookMode {
    fn from_value(value: &OsStr) -> Option<Self> {
        let value = value.to_str()?;
        if value.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else if value.eq_ignore_ascii_case("object") {
            Some(Self::Object)
        } else if value.eq_ignore_ascii_case("global-fallback")
            || value.eq_ignore_ascii_case("global_fallback")
            || value.eq_ignore_ascii_case("global")
        {
            Some(Self::GlobalFallback)
        } else if value.eq_ignore_ascii_case("observe") {
            Some(Self::Observe)
        } else if value.eq_ignore_ascii_case("off") {
            Some(Self::Off)
        } else {
            None
        }
    }
}

impl fmt::Display for HookMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Object => "object",
            Self::GlobalFallback => "global-fallback",
            Self::Observe => "observe",
            Self::Off => "off",
        })
    }
}

/// Gates progressively larger portions of the runtime during recovery.
///
/// Variant order is intentional: a stage permits itself and every earlier
/// stage according to [`Self::allows`].
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum SafeModeStage {
    /// Forward proxy exports only; do not install hooks.
    ProxyOnly,
    /// Install hooks but do not issue overlay rendering work.
    HooksOnly,
    /// Render only the non-interactive compositor probe.
    RenderProbe,
    /// Render the Nexus-owned core UI, but do not invoke addons.
    CoreUi,
    /// Enable the complete runtime, including addons.
    #[default]
    Addons,
}

impl SafeModeStage {
    /// Returns whether this stage permits the requested stage.
    #[must_use]
    pub const fn allows(self, requested: Self) -> bool {
        requested as u8 <= self as u8
    }

    fn from_value(value: &OsStr) -> Option<Self> {
        let value = value.to_str()?;
        if value.eq_ignore_ascii_case("proxy-only") || value.eq_ignore_ascii_case("proxy_only") {
            Some(Self::ProxyOnly)
        } else if value.eq_ignore_ascii_case("hooks-only")
            || value.eq_ignore_ascii_case("hooks_only")
        {
            Some(Self::HooksOnly)
        } else if value.eq_ignore_ascii_case("render-probe")
            || value.eq_ignore_ascii_case("render_probe")
        {
            Some(Self::RenderProbe)
        } else if value.eq_ignore_ascii_case("core-ui") || value.eq_ignore_ascii_case("core_ui") {
            Some(Self::CoreUi)
        } else if value.eq_ignore_ascii_case("addons") {
            Some(Self::Addons)
        } else {
            None
        }
    }
}

impl fmt::Display for SafeModeStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProxyOnly => "proxy-only",
            Self::HooksOnly => "hooks-only",
            Self::RenderProbe => "render-probe",
            Self::CoreUi => "core-ui",
            Self::Addons => "addons",
        })
    }
}

/// Legacy `-mumble` behavior, including the distinction between absence and
/// an option supplied without a value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum MumbleOption {
    /// `-mumble` was not supplied.
    #[default]
    Unspecified,
    /// `-mumble` was supplied with an empty or missing value.
    Default,
    /// `-mumble 0` disables Mumble polling while preserving shared resources.
    Disabled,
    /// A caller-supplied shared-memory name.
    Named(RedactedArg),
}

impl MumbleOption {
    /// Returns whether the command line explicitly contained `-mumble`.
    #[must_use]
    pub const fn was_requested(&self) -> bool {
        !matches!(self, Self::Unspecified)
    }

    /// Returns whether periodic Mumble polling should run.
    #[must_use]
    pub const fn polling_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Returns the exact reader name expected by the legacy integration.
    ///
    /// The returned value can contain user-controlled text and must not be
    /// logged.
    #[must_use]
    pub fn reader_name(&self) -> &OsStr {
        match self {
            Self::Unspecified | Self::Default => OsStr::new(DEFAULT_MUMBLE_NAME),
            Self::Disabled => OsStr::new("0"),
            Self::Named(value) => value.expose(),
        }
    }
}

/// Selects the addon configuration behavior requested by `-ggaddons`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum AddonSelection {
    /// Load the normal addon configuration.
    #[default]
    Default,
    /// Load a caller-supplied JSON configuration path.
    ConfigPath(RedactedArg),
    /// Use the normal configuration in read-only mode with these signatures.
    Whitelist(Vec<u32>),
}

impl AddonSelection {
    /// Returns a custom config path when one was selected.
    ///
    /// The returned path is user-controlled and must not be logged.
    #[must_use]
    pub fn config_path(&self) -> Option<&OsStr> {
        match self {
            Self::ConfigPath(path) => Some(path.expose()),
            Self::Default | Self::Whitelist(_) => None,
        }
    }

    /// Returns the legacy signature whitelist, preserving order and duplicates.
    #[must_use]
    pub fn whitelist(&self) -> Option<&[u32]> {
        match self {
            Self::Whitelist(signatures) => Some(signatures),
            Self::Default | Self::ConfigPath(_) => None,
        }
    }
}

/// Legacy multibox switches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MultiboxOptions {
    /// `-sharearchive` allows instances to share the archive.
    pub share_archive: bool,
    /// `-multi` enables the legacy local multibox behavior.
    pub share_local: bool,
}

/// Compatibility switches consumed by the original Nexus runtime.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LegacyOptions {
    /// `-ggvanilla` requests proxy forwarding without Nexus initialization.
    pub vanilla: bool,
    /// `-ggdev` requests the D3D11 debug device flag.
    pub debug_device: bool,
    /// `-ggconsole` requests a local console logger.
    pub console: bool,
    /// `-mumble` selection.
    pub mumble: MumbleOption,
    /// `-ggaddons` selection.
    pub addons: AddonSelection,
    /// Multibox compatibility switches.
    pub multibox: MultiboxOptions,
}

/// Explicit controls supplied by the user.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserOverrides {
    /// Optional hook-mode override, supplied with `-gghook`.
    pub hook_mode: Option<HookMode>,
    /// Optional safe-stage override, supplied with `-ggsafe`.
    pub safe_mode: Option<SafeModeStage>,
}

/// Parsed command-line configuration before safety constraints are applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ControlConfig {
    /// Compatibility options.
    pub legacy: LegacyOptions,
    /// Explicit modern controls.
    pub overrides: UserOverrides,
}

impl ControlConfig {
    /// Resolves requested controls into a coherent runtime mode.
    #[must_use]
    pub fn resolve(&self) -> RuntimeControls {
        let requested_hook_mode = self.overrides.hook_mode.unwrap_or_default();
        let requested_safe_mode = self.overrides.safe_mode.unwrap_or_default();

        if self.legacy.vanilla {
            return RuntimeControls {
                hook_mode: HookMode::Off,
                safe_mode: SafeModeStage::ProxyOnly,
                constrained_by: Some(ControlConstraint::LegacyVanilla),
            };
        }

        if requested_safe_mode == SafeModeStage::ProxyOnly {
            return RuntimeControls {
                hook_mode: HookMode::Off,
                safe_mode: SafeModeStage::ProxyOnly,
                constrained_by: Some(ControlConstraint::ProxyOnlyStage),
            };
        }

        if requested_hook_mode == HookMode::Off {
            return RuntimeControls {
                hook_mode: HookMode::Off,
                safe_mode: SafeModeStage::ProxyOnly,
                constrained_by: Some(ControlConstraint::HooksDisabled),
            };
        }

        if requested_hook_mode == HookMode::Observe
            && requested_safe_mode > SafeModeStage::HooksOnly
        {
            return RuntimeControls {
                hook_mode: HookMode::Observe,
                safe_mode: SafeModeStage::HooksOnly,
                constrained_by: Some(ControlConstraint::ObserveOnly),
            };
        }

        RuntimeControls {
            hook_mode: requested_hook_mode,
            safe_mode: requested_safe_mode,
            constrained_by: None,
        }
    }
}

/// Explains why effective controls differ from the requested controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlConstraint {
    /// The compatibility `-ggvanilla` switch forces proxy-only operation.
    LegacyVanilla,
    /// An explicit proxy-only safe stage prevents hook installation.
    ProxyOnlyStage,
    /// Hook mode `off` prevents every later safe-mode stage.
    HooksDisabled,
    /// Observe mode permits hooks but prevents render work.
    ObserveOnly,
}

/// Coherent controls ready for runtime consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeControls {
    /// Effective hook mode.
    pub hook_mode: HookMode,
    /// Effective safe-mode stage.
    pub safe_mode: SafeModeStage,
    /// Constraint that changed the requested controls, if any.
    pub constrained_by: Option<ControlConstraint>,
}

/// Identifies a command-line option without retaining its value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOption {
    /// `-ggaddons`.
    Addons,
    /// `-gghook`.
    HookMode,
    /// `-ggsafe`.
    SafeMode,
}

/// A non-fatal parse issue that contains no command-line value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlIssue {
    /// A value-bearing option had no following or attached value.
    MissingValue(ControlOption),
    /// A value did not match the option's closed set of supported values.
    InvalidValue(ControlOption),
    /// One `-ggaddons` list item did not begin with a legacy integer.
    InvalidAddonId,
    /// One `-ggaddons` list item exceeded the legacy signed 32-bit range.
    AddonIdOutOfRange,
}

/// Complete parse result, including safe issues that can be reported verbatim.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandLineParse {
    /// Parsed controls.
    pub config: ControlConfig,
    /// Non-fatal issues. These variants never contain raw argument data.
    pub issues: Vec<ControlIssue>,
}

impl CommandLineParse {
    /// Resolves this parse into coherent runtime controls.
    #[must_use]
    pub fn resolve(&self) -> RuntimeControls {
        self.config.resolve()
    }
}

/// Parses Windows-style, ASCII option names case-insensitively.
///
/// A single hyphen, double hyphen, or slash prefix is accepted. Values may be
/// supplied in the next argument or attached with `=` or `:`. Unknown options
/// are ignored because the host game owns the rest of the command line.
///
/// # Examples
///
/// ```
/// use nexus_control::{HookMode, SafeModeStage, parse_args};
///
/// let parsed = parse_args(["Gw2-64.exe", "/GGHOOK:object", "-GGSAFE=core-ui"]);
/// let controls = parsed.resolve();
/// assert_eq!(controls.hook_mode, HookMode::Object);
/// assert_eq!(controls.safe_mode, SafeModeStage::CoreUi);
/// ```
#[must_use]
pub fn parse_args<I, S>(args: I) -> CommandLineParse
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let command_line = ParsedCommandLine::new(args);
    let mut issues = Vec::new();

    let legacy = LegacyOptions {
        vanilla: command_line.has("ggvanilla"),
        debug_device: command_line.has("ggdev"),
        console: command_line.has("ggconsole"),
        mumble: parse_mumble(&command_line),
        addons: parse_addons(&command_line, &mut issues),
        multibox: MultiboxOptions {
            share_archive: command_line.has("sharearchive"),
            share_local: command_line.has("multi"),
        },
    };

    let overrides = UserOverrides {
        hook_mode: parse_hook_mode(&command_line, &mut issues),
        safe_mode: parse_safe_mode(&command_line, &mut issues),
    };

    CommandLineParse {
        config: ControlConfig { legacy, overrides },
        issues,
    }
}

fn parse_mumble(command_line: &ParsedCommandLine) -> MumbleOption {
    match command_line.value("mumble") {
        None => MumbleOption::Unspecified,
        Some(RawValue::Missing) => MumbleOption::Default,
        Some(RawValue::Value(value)) if value.is_empty() => MumbleOption::Default,
        Some(RawValue::Value(value)) if value == OsStr::new("0") => MumbleOption::Disabled,
        Some(RawValue::Value(value)) => MumbleOption::Named(RedactedArg::new(value)),
    }
}

fn parse_addons(
    command_line: &ParsedCommandLine,
    issues: &mut Vec<ControlIssue>,
) -> AddonSelection {
    let value = match command_line.value("ggaddons") {
        None => return AddonSelection::Default,
        Some(RawValue::Missing) => {
            issues.push(ControlIssue::MissingValue(ControlOption::Addons));
            return AddonSelection::Default;
        }
        Some(RawValue::Value(value)) => value,
    };

    let lossy = value.to_string_lossy();
    let entries: Vec<&str> = lossy.split(',').collect();
    if entries.len() == 1 && entries[0].contains(".json") {
        return AddonSelection::ConfigPath(RedactedArg::new(value));
    }

    let mut signatures = Vec::with_capacity(entries.len());
    for entry in entries {
        match parse_legacy_addon_id(entry) {
            Ok(signature) => signatures.push(signature),
            Err(LegacyIntegerError::Invalid) => issues.push(ControlIssue::InvalidAddonId),
            Err(LegacyIntegerError::OutOfRange) => {
                issues.push(ControlIssue::AddonIdOutOfRange);
            }
        }
    }

    if signatures.is_empty() {
        AddonSelection::Default
    } else {
        AddonSelection::Whitelist(signatures)
    }
}

fn parse_hook_mode(
    command_line: &ParsedCommandLine,
    issues: &mut Vec<ControlIssue>,
) -> Option<HookMode> {
    parse_override(
        command_line.value("gghook"),
        ControlOption::HookMode,
        HookMode::from_value,
        issues,
    )
}

fn parse_safe_mode(
    command_line: &ParsedCommandLine,
    issues: &mut Vec<ControlIssue>,
) -> Option<SafeModeStage> {
    parse_override(
        command_line.value("ggsafe"),
        ControlOption::SafeMode,
        SafeModeStage::from_value,
        issues,
    )
}

fn parse_override<T>(
    value: Option<RawValue<'_>>,
    option: ControlOption,
    parser: impl FnOnce(&OsStr) -> Option<T>,
    issues: &mut Vec<ControlIssue>,
) -> Option<T> {
    match value {
        None => None,
        Some(RawValue::Missing) => {
            issues.push(ControlIssue::MissingValue(option));
            None
        }
        Some(RawValue::Value(value)) => match parser(value) {
            Some(parsed) => Some(parsed),
            None => {
                issues.push(ControlIssue::InvalidValue(option));
                None
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyIntegerError {
    Invalid,
    OutOfRange,
}

fn parse_legacy_addon_id(value: &str) -> Result<u32, LegacyIntegerError> {
    let value = value.trim_start_matches(char::is_whitespace);
    let (negative, unsigned) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    if unsigned.is_empty() {
        return Err(LegacyIntegerError::Invalid);
    }

    let bytes = unsigned.as_bytes();
    let hexadecimal_prefix = bytes.len() > 2
        && bytes[0] == b'0'
        && matches!(bytes[1], b'x' | b'X')
        && bytes[2].is_ascii_hexdigit();
    let (base, digits) = if hexadecimal_prefix {
        (16_u64, &unsigned[2..])
    } else if bytes[0] == b'0' {
        (8_u64, unsigned)
    } else {
        (10_u64, unsigned)
    };

    let mut parsed_any = false;
    let mut magnitude = 0_u64;
    let limit = if negative {
        i32::MAX as u64 + 1
    } else {
        i32::MAX as u64
    };

    for byte in digits.bytes() {
        let Some(digit) = ascii_digit(byte) else {
            break;
        };
        if u64::from(digit) >= base {
            break;
        }
        parsed_any = true;
        magnitude = magnitude
            .checked_mul(base)
            .and_then(|number| number.checked_add(u64::from(digit)))
            .ok_or(LegacyIntegerError::OutOfRange)?;
        if magnitude > limit {
            return Err(LegacyIntegerError::OutOfRange);
        }
    }

    if !parsed_any {
        return Err(LegacyIntegerError::Invalid);
    }

    let signed = if negative {
        -(magnitude as i64)
    } else {
        magnitude as i64
    };
    Ok((signed as i32) as u32)
}

const fn ascii_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct ParsedCommandLine {
    args: Vec<OsString>,
    options: Vec<OptionToken>,
}

impl ParsedCommandLine {
    fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let options = args
            .iter()
            .enumerate()
            .filter_map(|(index, argument)| OptionToken::parse(index, argument))
            .collect();
        Self { args, options }
    }

    fn has(&self, name: &str) -> bool {
        self.options.iter().any(|option| option.name == name)
    }

    fn value(&self, name: &str) -> Option<RawValue<'_>> {
        let option = self.options.iter().find(|option| option.name == name)?;
        if let Some(value) = option.attached.as_deref() {
            return Some(RawValue::Value(value));
        }
        Some(match self.args.get(option.index + 1) {
            Some(value) => RawValue::Value(value),
            None => RawValue::Missing,
        })
    }
}

struct OptionToken {
    index: usize,
    name: String,
    attached: Option<OsString>,
}

impl OptionToken {
    fn parse(index: usize, argument: &OsStr) -> Option<Self> {
        let argument = argument.to_string_lossy();
        let body = argument
            .strip_prefix("--")
            .or_else(|| argument.strip_prefix('-'))
            .or_else(|| argument.strip_prefix('/'))?;
        if body.is_empty() {
            return None;
        }

        let separator = body
            .char_indices()
            .find(|(_, character)| matches!(character, '=' | ':'));
        let (name, attached) = match separator {
            Some((position, _)) => (
                &body[..position],
                Some(OsString::from(&body[position + 1..])),
            ),
            None => (body, None),
        };
        if name.is_empty() || !name.is_ascii() {
            return None;
        }

        Some(Self {
            index,
            name: name.to_ascii_lowercase(),
            attached,
        })
    }
}

enum RawValue<'a> {
    Missing,
    Value(&'a OsStr),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_legacy_switch_case_insensitively() {
        let parsed = parse_args([
            "Gw2-64.exe",
            "-GGVANILLA",
            "/GGDEV",
            "--GGCONSOLE",
            "/MUMBLE:Squad",
            "-SHAREARCHIVE",
            "/MULTI",
            "-GGADDONS",
            "0x2A,-1,077",
        ]);

        assert!(parsed.config.legacy.vanilla);
        assert!(parsed.config.legacy.debug_device);
        assert!(parsed.config.legacy.console);
        assert!(parsed.config.legacy.mumble.was_requested());
        assert_eq!(
            parsed.config.legacy.mumble.reader_name(),
            OsStr::new("Squad")
        );
        assert_eq!(
            parsed.config.legacy.addons.whitelist(),
            Some([42, u32::MAX, 63].as_slice())
        );
        assert_eq!(
            parsed.config.legacy.multibox,
            MultiboxOptions {
                share_archive: true,
                share_local: true,
            }
        );
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn independent_legacy_lookups_do_not_hide_following_flags() {
        let parsed = parse_args(["game.exe", "-mumble", "-multi"]);

        assert_eq!(
            parsed.config.legacy.mumble.reader_name(),
            OsStr::new("-multi")
        );
        assert!(parsed.config.legacy.multibox.share_local);
    }

    #[test]
    fn missing_mumble_value_preserves_default_reader_behavior() {
        let parsed = parse_args(["game.exe", "-mumble"]);

        assert_eq!(parsed.config.legacy.mumble, MumbleOption::Default);
        assert_eq!(
            parsed.config.legacy.mumble.reader_name(),
            OsStr::new(DEFAULT_MUMBLE_NAME)
        );
        assert!(parsed.config.legacy.mumble.polling_enabled());
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn mumble_zero_disables_polling() {
        let parsed = parse_args(["game.exe", "-mumble", "0"]);

        assert_eq!(parsed.config.legacy.mumble, MumbleOption::Disabled);
        assert!(!parsed.config.legacy.mumble.polling_enabled());
    }

    #[test]
    fn json_addon_override_is_redacted_by_default() {
        let marker = "DO_NOT_LOG_ARGUMENT_VALUE.json";
        let parsed = parse_args(["game.exe", "-ggaddons", marker]);
        let selection = &parsed.config.legacy.addons;

        assert_eq!(selection.config_path(), Some(OsStr::new(marker)));
        assert!(!format!("{selection:?}").contains(marker));
        assert_eq!(format!("{:?}", RedactedArg::new(marker)), REDACTED);
        assert_eq!(RedactedArg::new(marker).to_string(), REDACTED);
    }

    #[test]
    fn addon_ids_match_stoi_prefix_and_signed_behavior() {
        let parsed = parse_args([
            "game.exe",
            "-ggaddons",
            "123tail,+17,-2147483648,0x10tail,09",
        ]);

        assert_eq!(
            parsed.config.legacy.addons.whitelist(),
            Some([123, 17, 2_147_483_648, 16, 0].as_slice())
        );
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn invalid_addon_values_report_only_closed_issue_types() {
        let marker = "DO_NOT_LOG_ARGUMENT_VALUE";
        let parsed = parse_args([
            "game.exe",
            "-ggaddons",
            marker,
            "-gghook",
            marker,
            "-ggsafe",
            marker,
        ]);

        assert_eq!(parsed.config.legacy.addons, AddonSelection::Default);
        assert!(parsed.issues.contains(&ControlIssue::InvalidAddonId));
        assert!(
            parsed
                .issues
                .contains(&ControlIssue::InvalidValue(ControlOption::HookMode))
        );
        assert!(
            parsed
                .issues
                .contains(&ControlIssue::InvalidValue(ControlOption::SafeMode))
        );
        assert!(!format!("{:?}", parsed.issues).contains(marker));
    }

    #[test]
    fn parses_explicit_hook_and_safe_mode_overrides() {
        let parsed = parse_args([
            "game.exe",
            "-gghook=GLOBAL-FALLBACK",
            "/ggsafe:render-probe",
        ]);

        assert_eq!(
            parsed.config.overrides,
            UserOverrides {
                hook_mode: Some(HookMode::GlobalFallback),
                safe_mode: Some(SafeModeStage::RenderProbe),
            }
        );
        assert_eq!(
            parsed.resolve(),
            RuntimeControls {
                hook_mode: HookMode::GlobalFallback,
                safe_mode: SafeModeStage::RenderProbe,
                constrained_by: None,
            }
        );
    }

    #[test]
    fn vanilla_has_highest_safety_precedence() {
        let parsed = parse_args([
            "game.exe",
            "-ggvanilla",
            "-gghook",
            "object",
            "-ggsafe",
            "addons",
        ]);

        assert_eq!(
            parsed.resolve(),
            RuntimeControls {
                hook_mode: HookMode::Off,
                safe_mode: SafeModeStage::ProxyOnly,
                constrained_by: Some(ControlConstraint::LegacyVanilla),
            }
        );
    }

    #[test]
    fn observe_mode_cannot_enable_rendering() {
        let parsed = parse_args(["game.exe", "-gghook", "observe", "-ggsafe", "addons"]);

        assert_eq!(
            parsed.resolve(),
            RuntimeControls {
                hook_mode: HookMode::Observe,
                safe_mode: SafeModeStage::HooksOnly,
                constrained_by: Some(ControlConstraint::ObserveOnly),
            }
        );
    }

    #[test]
    fn proxy_only_stage_disables_hooks() {
        let parsed = parse_args([
            "game.exe",
            "-gghook",
            "global-fallback",
            "-ggsafe",
            "proxy-only",
        ]);

        assert_eq!(
            parsed.resolve(),
            RuntimeControls {
                hook_mode: HookMode::Off,
                safe_mode: SafeModeStage::ProxyOnly,
                constrained_by: Some(ControlConstraint::ProxyOnlyStage),
            }
        );
    }

    #[test]
    fn first_occurrence_wins_for_value_options() {
        let parsed = parse_args(["game.exe", "-gghook", "object", "-gghook", "off"]);

        assert_eq!(parsed.config.overrides.hook_mode, Some(HookMode::Object));
    }

    #[test]
    fn safe_mode_order_models_progressive_capabilities() {
        assert!(SafeModeStage::Addons.allows(SafeModeStage::ProxyOnly));
        assert!(SafeModeStage::CoreUi.allows(SafeModeStage::RenderProbe));
        assert!(!SafeModeStage::HooksOnly.allows(SafeModeStage::CoreUi));
    }
}
