use eframe::egui;
use st_protocol::{
    ControllerState, CursorShape, CursorState, InputCapabilities, KeyboardKey, StreamConfig,
    KEYBOARD_STATE_BYTES,
};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCaptureMode {
    Idle,
    HoverAbsolute,
    CapturedRelative,
    ForceReleased,
}

impl Default for LocalCaptureMode {
    fn default() -> Self {
        Self::Idle
    }
}

impl LocalCaptureMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::HoverAbsolute => "hover-absolute",
            Self::CapturedRelative => "captured-relative",
            Self::ForceReleased => "force-released",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SharedInputSnapshot {
    pub client_id: Option<u32>,
    pub controller_state: ControllerState,
    pub capabilities: InputCapabilities,
    pub stream_config: Option<StreamConfig>,
    pub cursor_shape: Option<CursorShape>,
    pub cursor_state: CursorState,
    pub cursor_shape_version: u64,
    pub cursor_state_version: u64,
}

impl Default for SharedInputSnapshot {
    fn default() -> Self {
        Self {
            client_id: None,
            controller_state: ControllerState::Unavailable,
            capabilities: InputCapabilities::default(),
            stream_config: None,
            cursor_shape: None,
            cursor_state: CursorState::default(),
            cursor_shape_version: 0,
            cursor_state_version: 0,
        }
    }
}

pub struct SharedInputState {
    inner: Mutex<SharedInputSnapshot>,
}

impl SharedInputState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SharedInputSnapshot::default()),
        }
    }

    pub fn reset(&self) {
        *self.inner.lock().unwrap() = SharedInputSnapshot::default();
    }

    pub fn snapshot(&self) -> SharedInputSnapshot {
        self.inner.lock().unwrap().clone()
    }

    pub fn set_client_id(&self, client_id: u32) {
        self.inner.lock().unwrap().client_id = Some(client_id);
    }

    pub fn set_controller_state(&self, controller_state: ControllerState) {
        self.inner.lock().unwrap().controller_state = controller_state;
    }

    pub fn set_capabilities(&self, capabilities: InputCapabilities) {
        self.inner.lock().unwrap().capabilities = capabilities;
    }

    pub fn set_stream_config(&self, stream_config: StreamConfig) {
        self.inner.lock().unwrap().stream_config = Some(stream_config);
    }

    pub fn set_cursor_shape(&self, cursor_shape: CursorShape) {
        let mut inner = self.inner.lock().unwrap();
        inner.cursor_shape = Some(cursor_shape);
        inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
    }

    pub fn set_cursor_state(&self, cursor_state: CursorState) {
        let mut inner = self.inner.lock().unwrap();
        inner.cursor_state = cursor_state;
        inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
    }
}

pub struct RemoteCursorTexture {
    pub hotspot: egui::Vec2,
    pub size: egui::Vec2,
    pub texture: egui::TextureHandle,
}

#[derive(Clone, Default)]
pub struct LocalKeyboardState {
    pressed: [u8; KEYBOARD_STATE_BYTES],
}

impl LocalKeyboardState {
    pub fn pressed(&self) -> [u8; KEYBOARD_STATE_BYTES] {
        self.pressed
    }

    pub fn clear(&mut self) -> bool {
        if self.pressed.iter().all(|byte| *byte == 0) {
            return false;
        }
        self.pressed = [0u8; KEYBOARD_STATE_BYTES];
        true
    }

    pub fn pressed_count(&self) -> usize {
        self.pressed
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum()
    }

    pub fn set_key(&mut self, key: KeyboardKey, pressed: bool) -> bool {
        let (byte, bit) = key.bit();
        let was_pressed = self.pressed[byte] & bit != 0;
        if was_pressed == pressed {
            return false;
        }
        if pressed {
            self.pressed[byte] |= bit;
        } else {
            self.pressed[byte] &= !bit;
        }
        true
    }

    pub fn sync_modifiers(&mut self, modifiers: egui::Modifiers) -> bool {
        let mut changed = false;
        changed |= self.set_key(KeyboardKey::LeftShift, modifiers.shift);
        changed |= self.set_key(KeyboardKey::LeftCtrl, modifiers.ctrl);
        changed |= self.set_key(KeyboardKey::LeftAlt, modifiers.alt);
        changed |= self.set_key(KeyboardKey::LeftMeta, modifiers.command);
        changed
    }
}
