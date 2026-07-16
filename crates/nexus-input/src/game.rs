use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use quick_xml::events::{BytesDecl, BytesStart, Event};
use quick_xml::{Reader, Writer, XmlVersion};

use crate::{
    GameBindId, GameInputError, GameSinkError, InputBind, InputDevice, LoadReport, Modifier,
    ModifierState, MouseButton, PersistenceError, known_game_binds,
};

const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const MAX_XML_ACTIONS: usize = 16_384;

/// Primary or secondary GW2 binding slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameSlot {
    /// Preferred binding.
    Primary,
    /// Fallback binding.
    Secondary,
}

/// The two binding slots exposed by GW2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MultiInputBind {
    /// Preferred binding.
    pub primary: InputBind,
    /// Fallback used when the primary is unbound.
    pub secondary: InputBind,
}

impl MultiInputBind {
    /// Returns the primary when bound, otherwise the secondary.
    #[must_use]
    pub const fn selected(self) -> Option<InputBind> {
        if self.primary.is_bound() {
            Some(self.primary)
        } else if self.secondary.is_bound() {
            Some(self.secondary)
        } else {
            None
        }
    }
}

/// Persistent GW2 binding registry with compatible XML preservation.
#[derive(Debug, Clone)]
pub struct GameBindRegistry {
    bindings: BTreeMap<GameBindId, MultiInputBind>,
    root_extra: BTreeMap<String, String>,
    action_extra: BTreeMap<GameBindId, BTreeMap<String, String>>,
    loaded_names: BTreeMap<GameBindId, String>,
}

impl Default for GameBindRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl GameBindRegistry {
    /// Builds the complete pinned default binding table.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self {
            bindings: known_game_binds()
                .iter()
                .filter(|entry| entry.id != GameBindId::LEGACY_MOVE_SWIM_UP)
                .map(|entry| (entry.id, MultiInputBind::default()))
                .collect(),
            root_extra: BTreeMap::new(),
            action_extra: BTreeMap::new(),
            loaded_names: BTreeMap::new(),
        };
        registry.apply_defaults();
        registry
    }

    /// Returns a binding pair.
    #[must_use]
    pub fn get(&self, action: GameBindId) -> Option<MultiInputBind> {
        self.bindings.get(&action.canonical()).copied()
    }

    /// Returns whether either slot is bound.
    #[must_use]
    pub fn is_bound(&self, action: GameBindId) -> bool {
        self.get(action)
            .and_then(MultiInputBind::selected)
            .is_some()
    }

    /// Sets one slot. The removed legacy swim-up action is ignored.
    pub fn set(&mut self, action: GameBindId, slot: GameSlot, binding: InputBind) -> bool {
        if action == GameBindId::LEGACY_MOVE_SWIM_UP {
            return false;
        }
        let entry = self.bindings.entry(action).or_default();
        match slot {
            GameSlot::Primary => entry.primary = binding.normalized(),
            GameSlot::Secondary => entry.secondary = binding.normalized(),
        }
        true
    }

    /// Returns all actions in deterministic numeric order.
    pub fn iter(&self) -> impl Iterator<Item = (GameBindId, MultiInputBind)> + '_ {
        self.bindings.iter().map(|(id, binding)| (*id, *binding))
    }

    /// Parses a compatible `GameBinds.xml` document atomically.
    pub fn load_xml(&mut self, source: &str) -> Result<LoadReport, PersistenceError> {
        if source.len() > MAX_XML_BYTES {
            return Err(PersistenceError::LimitExceeded);
        }
        let mut reader = Reader::from_str(source);
        reader.config_mut().trim_text(true);
        let mut candidate = Self::with_defaults();
        let mut depth = 0_usize;
        let mut saw_root = false;
        let mut report = LoadReport::default();
        let mut action_count = 0_usize;

        loop {
            match reader
                .read_event()
                .map_err(|_| PersistenceError::InvalidXml)?
            {
                Event::Decl(_) | Event::Comment(_) | Event::Text(_) => {}
                Event::Start(element) => {
                    if depth == 0 {
                        if element.name().as_ref() != b"InputBindings" {
                            return Err(PersistenceError::InvalidXml);
                        }
                        saw_root = true;
                        candidate.root_extra = parse_attributes(&reader, &element)?;
                    } else if depth == 1 && element.name().as_ref() == b"action" {
                        action_count = action_count.saturating_add(1);
                        if action_count > MAX_XML_ACTIONS {
                            return Err(PersistenceError::LimitExceeded);
                        }
                        let attributes = parse_attributes(&reader, &element)?;
                        if candidate.apply_xml_action(attributes) {
                            report.loaded += 1;
                        } else {
                            report.skipped += 1;
                        }
                    }
                    depth = depth.saturating_add(1);
                }
                Event::Empty(element) => {
                    if depth == 0 && element.name().as_ref() == b"InputBindings" {
                        saw_root = true;
                        candidate.root_extra = parse_attributes(&reader, &element)?;
                    } else if depth == 1 && element.name().as_ref() == b"action" {
                        action_count = action_count.saturating_add(1);
                        if action_count > MAX_XML_ACTIONS {
                            return Err(PersistenceError::LimitExceeded);
                        }
                        let attributes = parse_attributes(&reader, &element)?;
                        if candidate.apply_xml_action(attributes) {
                            report.loaded += 1;
                        } else {
                            report.skipped += 1;
                        }
                    }
                }
                Event::End(_) => {
                    depth = depth.checked_sub(1).ok_or(PersistenceError::InvalidXml)?;
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if !saw_root || depth != 0 {
            return Err(PersistenceError::InvalidXml);
        }
        *self = candidate;
        Ok(report)
    }

    fn apply_xml_action(&mut self, mut attributes: BTreeMap<String, String>) -> bool {
        let Some(id) = attributes
            .get("id")
            .and_then(|value| value.parse::<u32>().ok())
            .map(GameBindId)
        else {
            return false;
        };
        if id == GameBindId::LEGACY_MOVE_SWIM_UP {
            return false;
        }

        let primary = parse_xml_slot(&attributes, "", "", "");
        let secondary = parse_xml_slot(&attributes, "2", "2", "2");
        let binding = self.bindings.entry(id).or_default();
        if let Some(primary) = primary {
            binding.primary = primary;
        }
        if let Some(secondary) = secondary {
            binding.secondary = secondary;
        }

        if let Some(name) = attributes.remove("name") {
            self.loaded_names.insert(id, name);
        }
        for key in [
            "id", "device", "button", "mod", "device2", "button2", "mod2",
        ] {
            attributes.remove(key);
        }
        if !attributes.is_empty() {
            self.action_extra.insert(id, attributes);
        }
        true
    }

    /// Serializes compatible `GameBinds.xml` in deterministic action order.
    pub fn save_xml(&self) -> Result<String, PersistenceError> {
        let mut writer = Writer::new_with_indent(Vec::new(), b'\t', 1);
        writer
            .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .map_err(PersistenceError::Io)?;

        let mut root = BytesStart::new("InputBindings");
        for (key, value) in &self.root_extra {
            root.push_attribute((key.as_str(), value.as_str()));
        }
        writer
            .write_event(Event::Start(root))
            .map_err(PersistenceError::Io)?;

        for (id, binding) in &self.bindings {
            if *id == GameBindId::LEGACY_MOVE_SWIM_UP {
                continue;
            }
            let attributes = self.xml_action_attributes(*id, *binding);
            let mut action = BytesStart::new("action");
            for (key, value) in &attributes {
                action.push_attribute((key.as_str(), value.as_str()));
            }
            writer
                .write_event(Event::Empty(action))
                .map_err(PersistenceError::Io)?;
        }
        writer
            .write_event(Event::End(BytesStart::new("InputBindings").to_end()))
            .map_err(PersistenceError::Io)?;
        writer.get_mut().push(b'\n');
        String::from_utf8(writer.into_inner()).map_err(|_| PersistenceError::InvalidXml)
    }

    fn xml_action_attributes(
        &self,
        id: GameBindId,
        binding: MultiInputBind,
    ) -> Vec<(String, String)> {
        let mut attributes = Vec::new();
        let name = id
            .name()
            .map(str::to_owned)
            .or_else(|| self.loaded_names.get(&id).cloned())
            .unwrap_or_default();
        attributes.push(("name".to_owned(), name));
        attributes.push(("id".to_owned(), id.0.to_string()));
        append_xml_slot(&mut attributes, binding.primary, "", "", "");
        append_xml_slot(&mut attributes, binding.secondary, "2", "2", "2");
        if let Some(extra) = self.action_extra.get(&id) {
            let known: BTreeSet<String> = attributes.iter().map(|(key, _)| key.clone()).collect();
            attributes.extend(
                extra
                    .iter()
                    .filter(|(key, _)| !known.contains(*key))
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        attributes
    }

    /// Loads a bounded XML file.
    pub fn load_xml_file(&mut self, path: &Path) -> Result<LoadReport, PersistenceError> {
        let bytes = fs::read(path)?;
        if bytes.len() > MAX_XML_BYTES {
            return Err(PersistenceError::LimitExceeded);
        }
        let source = std::str::from_utf8(&bytes).map_err(|_| PersistenceError::InvalidXml)?;
        self.load_xml(source)
    }

    /// Writes the complete XML document and flushes file data.
    pub fn save_xml_file(&self, path: &Path) -> Result<(), PersistenceError> {
        let xml = self.save_xml()?;
        let mut file = fs::File::create(path)?;
        file.write_all(xml.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    fn apply_defaults(&mut self) {
        for default in DEFAULT_BOUND_BINDS {
            let primary = default.primary.to_input_bind();
            let secondary = default
                .secondary
                .map_or_else(InputBind::default, DefaultKey::to_input_bind);
            self.bindings
                .insert(default.action, MultiInputBind { primary, secondary });
        }
    }
}

/// Semantic game-window message. A platform sink translates this to native messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMessage {
    /// Modifier transition.
    Modifier {
        /// Modifier identity.
        modifier: Modifier,
        /// Down when true, up when false.
        pressed: bool,
        /// Alt uses system-key messages.
        system: bool,
    },
    /// Keyboard scan-code transition.
    Keyboard {
        /// Full Windows scan code.
        scan_code: u16,
        /// Down when true, up when false.
        pressed: bool,
        /// Whether to generate a system-key message.
        system: bool,
    },
    /// Mouse-button transition at the platform's current cursor position.
    Mouse {
        /// Nexus mouse button.
        button: MouseButton,
        /// Down when true, up when false.
        pressed: bool,
        /// Binding modifiers encoded in the native mouse `wParam`.
        modifiers: ModifierState,
    },
}

/// Platform input-state poll used for modifier restoration.
pub trait PhysicalInputState: Send + Sync + 'static {
    /// Returns the current physical modifier state.
    fn modifiers(&self) -> ModifierState;
}

/// Platform-owned game-message delivery boundary.
pub trait GameMessageSink: Send + Sync + 'static {
    /// Delivers one ordered batch. Implementations should avoid interleaving batches.
    fn send_batch(&self, messages: &[GameMessage]) -> Result<(), GameSinkError>;
}

/// Logical state reached by a game invocation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeState {
    /// Main key/button is held.
    Pressed,
    /// Main key/button was released.
    Released,
    /// Release is scheduled for a logical timestamp.
    Scheduled,
}

/// Closed result of a game invocation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameDispatch {
    /// Canonical action identifier.
    pub action: GameBindId,
    /// Resulting state.
    pub state: InvokeState,
    /// Number of semantic native messages sent.
    pub message_count: usize,
    /// Logical release timestamp for a scheduled invocation.
    pub release_due_millis: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ActivePress {
    binding: InputBind,
    release_due_millis: Option<u64>,
}

/// Deterministic press/release/invoke-duration state machine.
pub struct GameInvoker {
    registry: GameBindRegistry,
    sink: Arc<dyn GameMessageSink>,
    physical: Arc<dyn PhysicalInputState>,
    active: BTreeMap<GameBindId, ActivePress>,
}

impl GameInvoker {
    /// Creates a state machine around an injected platform boundary.
    #[must_use]
    pub fn new(
        registry: GameBindRegistry,
        sink: Arc<dyn GameMessageSink>,
        physical: Arc<dyn PhysicalInputState>,
    ) -> Self {
        Self {
            registry,
            sink,
            physical,
            active: BTreeMap::new(),
        }
    }

    /// Returns the game binding registry.
    #[must_use]
    pub const fn registry(&self) -> &GameBindRegistry {
        &self.registry
    }

    /// Mutates bindings. Active presses retain their original key for safe release.
    pub fn registry_mut(&mut self) -> &mut GameBindRegistry {
        &mut self.registry
    }

    /// Presses an action once.
    pub fn press(&mut self, action: GameBindId) -> Result<GameDispatch, GameInputError> {
        let action = action.canonical();
        if self.active.contains_key(&action) {
            return Err(GameInputError::AlreadyPressed);
        }
        let binding = self
            .registry
            .get(action)
            .and_then(MultiInputBind::selected)
            .ok_or(GameInputError::Unbound)?;
        let physical = self.poll_modifiers()?;
        let messages = press_messages(binding, physical)?;
        self.send(&messages)?;
        self.active.insert(
            action,
            ActivePress {
                binding,
                release_due_millis: None,
            },
        );
        Ok(GameDispatch {
            action,
            state: InvokeState::Pressed,
            message_count: messages.len(),
            release_due_millis: None,
        })
    }

    /// Releases an action. If it was not tracked, current bindings are used for API parity.
    pub fn release(&mut self, action: GameBindId) -> Result<GameDispatch, GameInputError> {
        let action = action.canonical();
        let binding = self
            .active
            .get(&action)
            .map(|active| active.binding)
            .or_else(|| self.registry.get(action).and_then(MultiInputBind::selected))
            .ok_or(GameInputError::Unbound)?;
        let physical = self.poll_modifiers()?;
        let messages = release_messages(binding, physical)?;
        self.send(&messages)?;
        self.active.remove(&action);
        Ok(GameDispatch {
            action,
            state: InvokeState::Released,
            message_count: messages.len(),
            release_due_millis: None,
        })
    }

    /// Presses now and releases immediately or after a logical duration.
    pub fn invoke(
        &mut self,
        action: GameBindId,
        duration_millis: u64,
        now_millis: u64,
    ) -> Result<GameDispatch, GameInputError> {
        let pressed = self.press(action)?;
        if duration_millis == 0 {
            let released = self.release(pressed.action)?;
            return Ok(GameDispatch {
                action: pressed.action,
                state: InvokeState::Released,
                message_count: pressed.message_count + released.message_count,
                release_due_millis: None,
            });
        }
        let due = now_millis.saturating_add(duration_millis);
        if let Some(active) = self.active.get_mut(&pressed.action) {
            active.release_due_millis = Some(due);
        }
        Ok(GameDispatch {
            action: pressed.action,
            state: InvokeState::Scheduled,
            message_count: pressed.message_count,
            release_due_millis: Some(due),
        })
    }

    /// Releases all scheduled invocations due at or before `now_millis`.
    pub fn advance(&mut self, now_millis: u64) -> Result<Vec<GameDispatch>, GameInputError> {
        let due: Vec<GameBindId> = self
            .active
            .iter()
            .filter(|(_, active)| {
                active
                    .release_due_millis
                    .is_some_and(|release| release <= now_millis)
            })
            .map(|(action, _)| *action)
            .collect();
        due.into_iter().map(|action| self.release(action)).collect()
    }

    /// Releases every tracked action, for focus loss and shutdown.
    pub fn release_all(&mut self) -> Result<Vec<GameDispatch>, GameInputError> {
        let actions: Vec<GameBindId> = self.active.keys().copied().collect();
        actions
            .into_iter()
            .map(|action| self.release(action))
            .collect()
    }

    /// Returns whether this state machine currently holds an action.
    #[must_use]
    pub fn is_pressed(&self, action: GameBindId) -> bool {
        self.active.contains_key(&action.canonical())
    }

    fn poll_modifiers(&self) -> Result<ModifierState, GameInputError> {
        catch_unwind(AssertUnwindSafe(|| self.physical.modifiers()))
            .map_err(|_| GameInputError::SinkPanicked)
    }

    fn send(&self, messages: &[GameMessage]) -> Result<(), GameInputError> {
        catch_unwind(AssertUnwindSafe(|| self.sink.send_batch(messages)))
            .map_err(|_| GameInputError::SinkPanicked)?
            .map_err(|_| GameInputError::SinkFailed)
    }
}

fn press_messages(
    binding: InputBind,
    physical: ModifierState,
) -> Result<Vec<GameMessage>, GameInputError> {
    let desired = binding.modifiers();
    let mut messages = Vec::with_capacity(4);
    for modifier in [Modifier::Alt, Modifier::Control, Modifier::Shift] {
        if desired.get(modifier) {
            messages.push(modifier_message(modifier, true));
        } else if physical.get(modifier) {
            messages.push(modifier_message(modifier, false));
        }
    }
    messages.push(main_message(binding, true)?);
    Ok(messages)
}

fn release_messages(
    binding: InputBind,
    physical: ModifierState,
) -> Result<Vec<GameMessage>, GameInputError> {
    let mut messages = Vec::with_capacity(4);
    messages.push(main_message(binding, false)?);
    for modifier in [Modifier::Alt, Modifier::Control, Modifier::Shift] {
        messages.push(modifier_message(modifier, physical.get(modifier)));
    }
    Ok(messages)
}

const fn modifier_message(modifier: Modifier, pressed: bool) -> GameMessage {
    GameMessage::Modifier {
        modifier,
        pressed,
        system: matches!(modifier, Modifier::Alt),
    }
}

fn main_message(binding: InputBind, pressed: bool) -> Result<GameMessage, GameInputError> {
    if binding.device == InputDevice::KEYBOARD {
        Ok(GameMessage::Keyboard {
            scan_code: binding.code,
            pressed,
            system: binding.alt,
        })
    } else if binding.device == InputDevice::MOUSE {
        let button =
            MouseButton::from_code(binding.code).filter(|button| *button != MouseButton::None);
        button
            .map(|button| GameMessage::Mouse {
                button,
                pressed,
                modifiers: binding.modifiers(),
            })
            .ok_or(GameInputError::Unbound)
    } else {
        Err(GameInputError::Unbound)
    }
}

fn parse_attributes(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<BTreeMap<String, String>, PersistenceError> {
    let mut attributes = BTreeMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| PersistenceError::InvalidXml)?;
        let key = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| PersistenceError::InvalidXml)?
            .to_owned();
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| PersistenceError::InvalidXml)?
            .into_owned();
        attributes.insert(key, value);
    }
    Ok(attributes)
}

fn parse_xml_slot(
    attributes: &BTreeMap<String, String>,
    device_suffix: &str,
    button_suffix: &str,
    modifier_suffix: &str,
) -> Option<InputBind> {
    let device_key = format!("device{device_suffix}");
    let button_key = format!("button{button_suffix}");
    let modifier_key = format!("mod{modifier_suffix}");
    let device = attributes.get(&device_key)?;
    let modifier_bits = attributes
        .get(&modifier_key)
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    let (input_device, code) = match device.as_str() {
        "Keyboard" => {
            let game_code = attributes.get(&button_key)?.parse::<u16>().ok()?;
            (
                InputDevice::KEYBOARD,
                game_scan_code_to_scan_code(game_code),
            )
        }
        "Mouse" => {
            let code = match attributes.get(&button_key)?.as_str() {
                "0" => MouseButton::Left as u16,
                "2" => MouseButton::Right as u16,
                "1" => MouseButton::Middle as u16,
                "3" => MouseButton::X1 as u16,
                "4" => MouseButton::X2 as u16,
                _ => return Some(InputBind::default()),
            };
            (InputDevice::MOUSE, code)
        }
        "None" => return Some(InputBind::default()),
        _ => return Some(InputBind::default()),
    };
    Some(
        InputBind::new(
            modifier_bits & 0b0100 != 0,
            modifier_bits & 0b0010 != 0,
            modifier_bits & 0b0001 != 0,
            input_device,
            code,
        )
        .normalized(),
    )
}

fn append_xml_slot(
    attributes: &mut Vec<(String, String)>,
    binding: InputBind,
    device_suffix: &str,
    button_suffix: &str,
    modifier_suffix: &str,
) {
    let device_key = format!("device{device_suffix}");
    let button_key = format!("button{button_suffix}");
    let modifier_key = format!("mod{modifier_suffix}");
    if binding.device == InputDevice::KEYBOARD && binding.code != 0 {
        attributes.push((device_key, "Keyboard".to_owned()));
        attributes.push((
            button_key,
            scan_code_to_game_scan_code(binding.code).to_string(),
        ));
    } else if binding.device == InputDevice::MOUSE && binding.code != 0 {
        attributes.push((device_key, "Mouse".to_owned()));
        if let Some(button) = mouse_button_to_game(binding.code) {
            attributes.push((button_key, button.to_owned()));
        }
    } else {
        attributes.push((device_key, "None".to_owned()));
    }
    let modifier_bits =
        u8::from(binding.shift) + (u8::from(binding.control) << 1) + (u8::from(binding.alt) << 2);
    if modifier_bits != 0 {
        attributes.push((modifier_key, modifier_bits.to_string()));
    }
}

const fn mouse_button_to_game(code: u16) -> Option<&'static str> {
    match MouseButton::from_code(code) {
        Some(MouseButton::Left) => Some("0"),
        Some(MouseButton::Right) => Some("2"),
        Some(MouseButton::Middle) => Some("1"),
        Some(MouseButton::X1) => Some("3"),
        Some(MouseButton::X2) => Some("4"),
        _ => None,
    }
}

const GAME_SCAN_CODES: &[(u16, u16)] = &[
    (0, 0x38),
    (1, 0x1D),
    (2, 0x2A),
    (3, 0x28),
    (4, 0x2B),
    (5, 0x3A),
    (6, 0x33),
    (7, 0x0C),
    (8, 0x0D),
    (9, 0x01),
    (10, 0x1A),
    (11, 0xE045),
    (12, 0x34),
    (13, 0x1B),
    (14, 0x27),
    (15, 0x35),
    (16, 0xE037),
    (17, 0x29),
    (18, 0x0E),
    (19, 0xE053),
    (20, 0x1C),
    (21, 0x39),
    (22, 0x0F),
    (23, 0xE04F),
    (24, 0xE047),
    (25, 0xE052),
    (26, 0xE051),
    (27, 0xE049),
    (28, 0xE050),
    (29, 0xE04B),
    (30, 0xE04D),
    (31, 0xE048),
    (32, 0x3B),
    (33, 0x3C),
    (34, 0x3D),
    (35, 0x3E),
    (36, 0x3F),
    (37, 0x40),
    (38, 0x41),
    (39, 0x42),
    (40, 0x43),
    (41, 0x44),
    (42, 0x57),
    (43, 0x58),
    (48, 0x0B),
    (49, 0x02),
    (50, 0x03),
    (51, 0x04),
    (52, 0x05),
    (53, 0x06),
    (54, 0x07),
    (55, 0x08),
    (56, 0x09),
    (57, 0x0A),
    (65, 0x1E),
    (66, 0x30),
    (67, 0x2E),
    (68, 0x20),
    (69, 0x12),
    (70, 0x21),
    (71, 0x22),
    (72, 0x23),
    (73, 0x17),
    (74, 0x24),
    (75, 0x25),
    (76, 0x26),
    (77, 0x32),
    (78, 0x31),
    (79, 0x18),
    (80, 0x19),
    (81, 0x10),
    (82, 0x13),
    (83, 0x1F),
    (84, 0x14),
    (85, 0x16),
    (86, 0x2F),
    (87, 0x11),
    (88, 0x2D),
    (89, 0x15),
    (90, 0x2C),
    (91, 0x4E),
    (92, 0x53),
    (93, 0xE035),
    (94, 0x37),
    (95, 0x52),
    (96, 0x4F),
    (97, 0x50),
    (98, 0x51),
    (99, 0x4B),
    (100, 0x4C),
    (101, 0x4D),
    (102, 0x47),
    (103, 0x48),
    (104, 0x49),
    (105, 0xE01C),
    (106, 0x4A),
    (109, 0xE038),
    (110, 0xE01D),
    (111, 0x56),
    (135, 0x36),
];

/// Converts ArenaNet's US-layout scan code to a full Windows scan code.
#[must_use]
pub fn game_scan_code_to_scan_code(game_scan_code: u16) -> u16 {
    GAME_SCAN_CODES
        .iter()
        .find(|(game, _)| *game == game_scan_code)
        .map_or(0, |(_, scan)| *scan)
}

/// Converts a full Windows scan code to ArenaNet's US-layout scan code.
#[must_use]
pub fn scan_code_to_game_scan_code(scan_code: u16) -> u16 {
    GAME_SCAN_CODES
        .iter()
        .find(|(_, scan)| *scan == scan_code)
        .map_or(0, |(game, _)| *game)
}

#[derive(Clone, Copy)]
struct DefaultKey {
    alt: bool,
    control: bool,
    shift: bool,
    game_scan_code: u16,
}

impl DefaultKey {
    fn to_input_bind(self) -> InputBind {
        InputBind::new(
            self.alt,
            self.control,
            self.shift,
            InputDevice::KEYBOARD,
            game_scan_code_to_scan_code(self.game_scan_code),
        )
    }
}

#[derive(Clone, Copy)]
struct DefaultBinding {
    action: GameBindId,
    primary: DefaultKey,
    secondary: Option<DefaultKey>,
}

const fn key(game_scan_code: u16) -> DefaultKey {
    DefaultKey {
        alt: false,
        control: false,
        shift: false,
        game_scan_code,
    }
}

const fn modified(alt: bool, control: bool, shift: bool, game_scan_code: u16) -> DefaultKey {
    DefaultKey {
        alt,
        control,
        shift,
        game_scan_code,
    }
}

const DEFAULT_BOUND_BINDS: &[DefaultBinding] = &[
    DefaultBinding {
        action: GameBindId::MOVE_FORWARD,
        primary: key(87),
        secondary: Some(key(31)),
    },
    DefaultBinding {
        action: GameBindId::MOVE_BACKWARD,
        primary: key(83),
        secondary: Some(key(28)),
    },
    DefaultBinding {
        action: GameBindId::MOVE_LEFT,
        primary: key(65),
        secondary: Some(key(29)),
    },
    DefaultBinding {
        action: GameBindId::MOVE_RIGHT,
        primary: key(68),
        secondary: Some(key(30)),
    },
    DefaultBinding {
        action: GameBindId::MOVE_TURN_LEFT,
        primary: key(81),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MOVE_TURN_RIGHT,
        primary: key(69),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MOVE_DODGE,
        primary: key(86),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MOVE_AUTO_RUN,
        primary: key(82),
        secondary: Some(key(11)),
    },
    DefaultBinding {
        action: GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP,
        primary: key(21),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_SWAP,
        primary: key(17),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_1,
        primary: key(49),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_2,
        primary: key(50),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_3,
        primary: key(51),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_4,
        primary: key(52),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_WEAPON_5,
        primary: key(53),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_HEAL,
        primary: key(54),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_UTILITY_1,
        primary: key(55),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_UTILITY_2,
        primary: key(56),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_UTILITY_3,
        primary: key(57),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_ELITE,
        primary: key(48),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_1,
        primary: key(32),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_2,
        primary: key(33),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_3,
        primary: key(34),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_4,
        primary: key(35),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_5,
        primary: key(36),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_6,
        primary: key(37),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_PROFESSION_7,
        primary: key(38),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SKILL_SPECIAL_ACTION,
        primary: key(78),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TARGET_ALERT,
        primary: modified(false, false, true, 84),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TARGET_CALL,
        primary: modified(false, true, false, 84),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TARGET_TAKE,
        primary: key(84),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TARGET_ENEMY_NEXT,
        primary: key(22),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TARGET_ENEMY_PREVIOUS,
        primary: modified(false, false, true, 22),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_COMMERCE,
        primary: key(79),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_CONTACTS,
        primary: key(89),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_GUILD,
        primary: key(71),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_HERO,
        primary: key(72),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_INVENTORY,
        primary: key(73),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_KENNEL,
        primary: key(75),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_LOGOUT,
        primary: key(43),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_OPTIONS,
        primary: key(42),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_PARTY,
        primary: key(80),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_SCOREBOARD,
        primary: key(66),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_SEASONAL_OBJECTIVES_SHOP,
        primary: modified(false, false, true, 72),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_INFORMATION,
        primary: key(7),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_CHAT_TOGGLE,
        primary: key(4),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_CHAT_COMMAND,
        primary: key(15),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_CHAT_FOCUS,
        primary: key(20),
        secondary: Some(key(105)),
    },
    DefaultBinding {
        action: GameBindId::UI_CHAT_REPLY,
        primary: key(18),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_TOGGLE,
        primary: modified(false, true, true, 72),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_SQUAD_BROADCAST_CHAT_TOGGLE,
        primary: modified(false, false, true, 4),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_SQUAD_BROADCAST_CHAT_COMMAND,
        primary: modified(false, false, true, 15),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::UI_SQUAD_BROADCAST_CHAT_FOCUS,
        primary: modified(false, false, true, 20),
        secondary: Some(modified(false, false, true, 105)),
    },
    DefaultBinding {
        action: GameBindId::CAMERA_ZOOM_IN,
        primary: key(27),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::CAMERA_ZOOM_OUT,
        primary: key(26),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SCREENSHOT_NORMAL,
        primary: key(16),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MAP_TOGGLE,
        primary: key(77),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MAP_FOCUS_PLAYER,
        primary: key(21),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MAP_FLOOR_DOWN,
        primary: key(26),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MAP_FLOOR_UP,
        primary: key(27),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MAP_ZOOM_IN,
        primary: key(91),
        secondary: Some(key(8)),
    },
    DefaultBinding {
        action: GameBindId::MAP_ZOOM_OUT,
        primary: key(106),
        secondary: Some(key(7)),
    },
    DefaultBinding {
        action: GameBindId::SPUMONI_TOGGLE,
        primary: key(88),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPUMONI_MOVEMENT,
        primary: key(86),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPUMONI_SECONDARY_MOVEMENT,
        primary: key(67),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_NEAREST_FIXED,
        primary: modified(false, false, true, 22),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_NEAREST_PLAYER,
        primary: key(22),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_RED_1,
        primary: key(49),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_RED_2,
        primary: key(50),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_RED_3,
        primary: key(51),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_RED_4,
        primary: key(52),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_RED_5,
        primary: key(53),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_BLUE_1,
        primary: key(54),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_BLUE_2,
        primary: key(55),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_BLUE_3,
        primary: key(56),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_BLUE_4,
        primary: key(57),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_PLAYER_BLUE_5,
        primary: key(48),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_CAMERA,
        primary: modified(false, true, true, 70),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_CAMERA_MODE,
        primary: key(69),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_FORWARD,
        primary: key(87),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_BACKWARD,
        primary: key(83),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_LEFT,
        primary: key(65),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_RIGHT,
        primary: key(68),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_UP,
        primary: key(21),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SPECTATOR_FREE_MOVE_DOWN,
        primary: key(86),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_1,
        primary: modified(true, false, false, 49),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_2,
        primary: modified(true, false, false, 50),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_3,
        primary: modified(true, false, false, 51),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_4,
        primary: modified(true, false, false, 52),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_5,
        primary: modified(true, false, false, 53),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_6,
        primary: modified(true, false, false, 54),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_7,
        primary: modified(true, false, false, 55),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_PLACE_WORLD_8,
        primary: modified(true, false, false, 56),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_CLEAR_ALL_WORLD,
        primary: modified(true, false, false, 57),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_1,
        primary: modified(true, false, true, 49),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_2,
        primary: modified(true, false, true, 50),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_3,
        primary: modified(true, false, true, 51),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_4,
        primary: modified(true, false, true, 52),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_5,
        primary: modified(true, false, true, 53),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_6,
        primary: modified(true, false, true, 54),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_7,
        primary: modified(true, false, true, 55),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_SET_AGENT_8,
        primary: modified(true, false, true, 56),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::SQUAD_MARKER_CLEAR_ALL_AGENT,
        primary: modified(true, false, true, 57),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MASTERY_ACCESS,
        primary: key(74),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_INTERACT,
        primary: key(70),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_SHOW_ENEMIES,
        primary: key(1),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_SHOW_ALLIES,
        primary: key(0),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_TOGGLE_LANGUAGE,
        primary: key(110),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_TOGGLE_FULL_SCREEN,
        primary: modified(true, false, false, 20),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::MISC_TOGGLE_DECORATION_MODE,
        primary: key(76),
        secondary: None,
    },
    DefaultBinding {
        action: GameBindId::TOY_USE_DEFAULT,
        primary: key(85),
        secondary: None,
    },
];

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        batches: Mutex<Vec<Vec<GameMessage>>>,
    }

    impl GameMessageSink for RecordingSink {
        fn send_batch(&self, messages: &[GameMessage]) -> Result<(), GameSinkError> {
            self.batches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(messages.to_vec());
            Ok(())
        }
    }

    struct FixedPhysical(ModifierState);

    impl PhysicalInputState for FixedPhysical {
        fn modifiers(&self) -> ModifierState {
            self.0
        }
    }

    fn batches(sink: &RecordingSink) -> MutexGuard<'_, Vec<Vec<GameMessage>>> {
        sink.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn full_scan_code_table_round_trips() {
        for (game, scan) in GAME_SCAN_CODES {
            assert_eq!(game_scan_code_to_scan_code(*game), *scan);
            assert_eq!(scan_code_to_game_scan_code(*scan), *game);
        }
        assert_eq!(game_scan_code_to_scan_code(u16::MAX), 0);
        assert_eq!(scan_code_to_game_scan_code(u16::MAX), 0);
    }

    #[test]
    fn default_table_contains_every_nonlegacy_known_action() {
        let registry = GameBindRegistry::with_defaults();
        assert_eq!(std::mem::size_of::<MultiInputBind>(), 24);
        assert_eq!(registry.iter().count() + 1, known_game_binds().len());
        assert_eq!(
            registry.get(GameBindId::MOVE_FORWARD),
            Some(MultiInputBind {
                primary: InputBind::new(false, false, false, InputDevice::KEYBOARD, 0x11),
                secondary: InputBind::new(false, false, false, InputDevice::KEYBOARD, 0xE048),
            })
        );
        assert!(registry.is_bound(GameBindId::SKILL_WEAPON_1));
        assert!(!registry.is_bound(GameBindId::MOVE_WALK));
    }

    #[test]
    fn xml_golden_load_save_preserves_unknown_attributes() {
        let source = r#"<?xml version="1.0"?>
<InputBindings schema="future">
  <action name="custom" id="18" device="Keyboard" button="49" mod="3" device2="Mouse" button2="4" future="kept" />
  <action name="unknown" id="9000" device="None" device2="None" custom="yes" />
</InputBindings>"#;
        let mut registry = GameBindRegistry::with_defaults();
        let report = registry.load_xml(source).expect("fixture XML should load");
        assert_eq!(report.loaded, 2);
        let skill = registry
            .get(GameBindId::SKILL_WEAPON_1)
            .expect("known action should exist");
        assert!(skill.primary.control);
        assert!(skill.primary.shift);
        assert_eq!(skill.primary.code, 0x02);
        assert_eq!(skill.secondary.code, MouseButton::X2 as u16);
        assert!(registry.get(GameBindId(9000)).is_some());

        let saved = registry.save_xml().expect("registry should serialize");
        assert!(saved.contains("schema=\"future\""));
        assert!(saved.contains("future=\"kept\""));
        assert!(saved.contains("custom=\"yes\""));
        assert!(saved.contains("id=\"18\""));
        assert!(!saved.contains("id=\"10\""));
    }

    #[test]
    fn malformed_xml_does_not_partially_replace_registry() {
        let mut registry = GameBindRegistry::with_defaults();
        let before = registry.get(GameBindId::MOVE_FORWARD);
        assert!(
            registry
                .load_xml("<wrong><action id=\"0\"/></wrong>")
                .is_err()
        );
        assert_eq!(registry.get(GameBindId::MOVE_FORWARD), before);
    }

    #[test]
    fn press_neutralizes_and_release_restores_physical_modifiers() {
        let sink = Arc::new(RecordingSink::default());
        let physical = Arc::new(FixedPhysical(ModifierState {
            alt: false,
            control: true,
            shift: false,
        }));
        let mut registry = GameBindRegistry::with_defaults();
        registry.set(
            GameBindId::SKILL_WEAPON_1,
            GameSlot::Primary,
            InputBind::new(true, false, false, InputDevice::KEYBOARD, 0x02),
        );
        let mut invoker = GameInvoker::new(registry, sink.clone(), physical);
        invoker
            .press(GameBindId::SKILL_WEAPON_1)
            .expect("press should dispatch");
        invoker
            .release(GameBindId::SKILL_WEAPON_1)
            .expect("release should dispatch");
        let recorded = batches(&sink);
        assert_eq!(
            recorded[0],
            [
                GameMessage::Modifier {
                    modifier: Modifier::Alt,
                    pressed: true,
                    system: true,
                },
                GameMessage::Modifier {
                    modifier: Modifier::Control,
                    pressed: false,
                    system: false,
                },
                GameMessage::Keyboard {
                    scan_code: 0x02,
                    pressed: true,
                    system: true,
                },
            ]
        );
        assert_eq!(recorded[1].len(), 4);
        assert_eq!(
            recorded[1][2],
            GameMessage::Modifier {
                modifier: Modifier::Control,
                pressed: true,
                system: false,
            }
        );
    }

    #[test]
    fn invoke_duration_uses_logical_time_without_sleeping() {
        let sink = Arc::new(RecordingSink::default());
        let physical = Arc::new(FixedPhysical(ModifierState::default()));
        let mut invoker = GameInvoker::new(GameBindRegistry::with_defaults(), sink, physical);
        let scheduled = invoker
            .invoke(GameBindId::SKILL_WEAPON_1, 50, 100)
            .expect("invoke should press and schedule");
        assert_eq!(scheduled.state, InvokeState::Scheduled);
        assert_eq!(scheduled.release_due_millis, Some(150));
        assert!(
            invoker
                .advance(149)
                .expect("advance should succeed")
                .is_empty()
        );
        assert_eq!(
            invoker.advance(150).expect("release should succeed")[0].state,
            InvokeState::Released
        );
        assert!(!invoker.is_pressed(GameBindId::SKILL_WEAPON_1));
    }

    #[test]
    fn legacy_swim_up_dispatches_merged_jump_binding() {
        let sink = Arc::new(RecordingSink::default());
        let physical = Arc::new(FixedPhysical(ModifierState::default()));
        let mut invoker = GameInvoker::new(GameBindRegistry::with_defaults(), sink, physical);
        let dispatch = invoker
            .press(GameBindId::LEGACY_MOVE_SWIM_UP)
            .expect("legacy alias should press merged action");
        assert_eq!(dispatch.action, GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP);
    }

    #[test]
    fn sink_panic_is_contained_without_marking_action_pressed() {
        struct PanickingSink;
        impl GameMessageSink for PanickingSink {
            fn send_batch(&self, _: &[GameMessage]) -> Result<(), GameSinkError> {
                panic!("test sink panic")
            }
        }
        let mut invoker = GameInvoker::new(
            GameBindRegistry::with_defaults(),
            Arc::new(PanickingSink),
            Arc::new(FixedPhysical(ModifierState::default())),
        );
        assert_eq!(
            invoker.press(GameBindId::SKILL_WEAPON_1),
            Err(GameInputError::SinkPanicked)
        );
        assert!(!invoker.is_pressed(GameBindId::SKILL_WEAPON_1));
    }
}
