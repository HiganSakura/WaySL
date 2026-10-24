//! Forwards Windows input to the compositor.
//!
//! The renderer owns the window, so it is the only side that sees keyboard and
//! pointer events. They are translated into Linux input codes and written to
//! the compositor's input port, where they become `wl_seat` events.
//!
//! Input is off unless `WAYSL_INPUT_FORWARD` names an address, so a deployment
//! that does not want input needs no extra configuration.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use bevy::window::{CursorMoved, Window};

use crate::frame_bridge::{InputEvent, InputWriter};
use crate::renderer::surfaces::{SurfaceId, SurfaceRegistry};

/// Linux input event code for the left mouse button.
const BTN_LEFT: u32 = 0x110;
/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;
/// Linux input event code for the middle mouse button.
const BTN_MIDDLE: u32 = 0x112;

/// Maps a Bevy mouse button to its Linux input event code.
fn button_code(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        // Side buttons and unknown buttons have no stable evdev code here.
        _ => return None,
    })
}

/// Resource holding the forwarder and the address it writes to.
#[derive(Resource)]
pub struct InputForwarder {
    address: Option<String>,
    writer: InputWriter,
}

impl InputForwarder {
    /// Creates a forwarder for `address`, or a disabled one when it is `None`.
    #[must_use]
    pub fn new(address: Option<String>) -> Self {
        Self {
            address,
            writer: InputWriter::default(),
        }
    }

    /// Sends `event` for `surface` if input is enabled.
    pub fn send(&mut self, surface: SurfaceId, event: InputEvent) {
        let Some(address) = self.address.as_deref() else {
            return;
        };
        if let Err(error) = self.writer.send(address, surface.0, event) {
            // A dropped connection is expected when the compositor restarts;
            // the next event reconnects.
            eprintln!("WaySL input forward failed: {error}");
        }
    }
}

/// Returns the surface a window belongs to, if it is one WaySL created.
///
/// The placeholder window and any window without a registry entry are ignored.
fn surface_for(registry: &SurfaceRegistry, entity: Entity) -> Option<SurfaceId> {
    registry
        .windows
        .iter()
        .find(|(_, window)| **window == entity)
        .map(|(surface, _)| *surface)
}

/// Forwards cursor movement as pointer motion.
///
/// Bevy reports the cursor in window coordinates from the top left, which is
/// what Wayland surface coordinates are too, so no flip is needed.
pub fn forward_cursor(
    mut forwarder: ResMut<InputForwarder>,
    registry: Res<SurfaceRegistry>,
    mut events: MessageReader<CursorMoved>,
) {
    for event in events.read() {
        let Some(surface) = surface_for(&registry, event.window) else {
            continue;
        };
        forwarder.send(
            surface,
            InputEvent::PointerMotion {
                x: event.position.x,
                y: event.position.y,
            },
        );
    }
}

/// Forwards mouse button presses and releases.
pub fn forward_buttons(
    mut forwarder: ResMut<InputForwarder>,
    registry: Res<SurfaceRegistry>,
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
) {
    // Buttons share the cursor, so they belong to the focused window.
    let Some(surface) = focused(&registry, &windows) else {
        return;
    };
    for button in buttons.get_just_pressed() {
        if let Some(code) = button_code(*button) {
            forwarder.send(
                surface,
                InputEvent::PointerButton {
                    button: code,
                    pressed: true,
                },
            );
        }
    }
    for button in buttons.get_just_released() {
        if let Some(code) = button_code(*button) {
            forwarder.send(
                surface,
                InputEvent::PointerButton {
                    button: code,
                    pressed: false,
                },
            );
        }
    }
}

/// Forwards the scroll wheel as pointer axis events.
pub fn forward_wheel(
    mut forwarder: ResMut<InputForwarder>,
    registry: Res<SurfaceRegistry>,
    mut wheel: MessageReader<MouseWheel>,
    windows: Query<&Window>,
) {
    // `MouseWheel` carries no window, so the event is attributed to the focused
    // one. Sending to every surface would scroll all of them at once.
    let Some(surface) = focused(&registry, &windows) else {
        return;
    };
    for event in wheel.read() {
        forwarder.send(
            surface,
            InputEvent::PointerAxis {
                horizontal: event.x,
                vertical: event.y,
            },
        );
    }
}

/// Reports whether the primary window is focused, used to gate key forwarding.
fn focused(registry: &SurfaceRegistry, windows: &Query<&Window>) -> Option<SurfaceId> {
    registry.windows.iter().find_map(|(surface, entity)| {
        let window = windows.get(*entity).ok()?;
        window.focused.then_some(*surface)
    })
}

/// Forwards keyboard input that Bevy exposes as text or named keys.
///
/// Only a small, unambiguous mapping is provided: printable ASCII and the
/// common editing and modifier keys. Extending it is mechanical, but every
/// entry has to be checked against Linux input codes, and a wrong code types
/// the wrong character rather than failing visibly.
pub fn forward_keys(
    mut forwarder: ResMut<InputForwarder>,
    registry: Res<SurfaceRegistry>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window>,
) {
    let Some(surface) = focused(&registry, &windows) else {
        return;
    };
    for key in keys.get_just_pressed() {
        if let Some(code) = key_code(*key) {
            forwarder.send(
                surface,
                InputEvent::Key {
                    key: code,
                    pressed: true,
                },
            );
        }
    }
    for key in keys.get_just_released() {
        if let Some(code) = key_code(*key) {
            forwarder.send(
                surface,
                InputEvent::Key {
                    key: code,
                    pressed: false,
                },
            );
        }
    }
}

/// Maps a Bevy key to a Linux keycode **minus 8**, which is what Wayland's
/// `wl_keyboard.key` expects (evdev codes start at 1 for Escape).
fn key_code(key: KeyCode) -> Option<u32> {
    // Bevy key -> evdev code, from `input-event-codes.h`.
    let evdev = match key {
        KeyCode::Escape => 1,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::Backquote => 41,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::NumpadMultiply => 55,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::CapsLock => 58,
        KeyCode::F1 => 59,
        KeyCode::F2 => 60,
        KeyCode::F3 => 61,
        KeyCode::F4 => 62,
        KeyCode::F5 => 63,
        KeyCode::F6 => 64,
        KeyCode::F7 => 65,
        KeyCode::F8 => 66,
        KeyCode::F9 => 67,
        KeyCode::F10 => 68,
        KeyCode::NumLock => 69,
        KeyCode::ScrollLock => 70,
        KeyCode::Numpad7 => 71,
        KeyCode::Numpad8 => 72,
        KeyCode::Numpad9 => 73,
        KeyCode::NumpadSubtract => 74,
        KeyCode::Numpad4 => 75,
        KeyCode::Numpad5 => 76,
        KeyCode::Numpad6 => 77,
        KeyCode::NumpadAdd => 78,
        KeyCode::Numpad1 => 79,
        KeyCode::Numpad2 => 80,
        KeyCode::Numpad3 => 81,
        KeyCode::Numpad0 => 82,
        KeyCode::NumpadDecimal => 83,
        KeyCode::F11 => 87,
        KeyCode::F12 => 88,
        KeyCode::NumpadEnter => 96,
        KeyCode::ControlRight => 97,
        KeyCode::NumpadDivide => 98,
        KeyCode::PrintScreen => 99,
        KeyCode::AltRight => 100,
        KeyCode::Home => 102,
        KeyCode::ArrowUp => 103,
        KeyCode::PageUp => 104,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowRight => 106,
        KeyCode::End => 107,
        KeyCode::ArrowDown => 108,
        KeyCode::PageDown => 109,
        KeyCode::Insert => 110,
        KeyCode::Delete => 111,
        KeyCode::SuperLeft => 125,
        KeyCode::SuperRight => 126,
        // Unmapped keys are dropped rather than guessed.
        _ => return None,
    };
    // Wayland expects the evdev code offset by 8.
    Some(evdev - 8)
}
