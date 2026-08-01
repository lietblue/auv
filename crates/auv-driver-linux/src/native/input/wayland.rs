use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::time::Instant;

use auv_driver_common::error::DriverResult;
use auv_driver_common::geometry::{Point, Rect};
use auv_driver_common::input::{Click, MouseButton, Scroll};
use tempfile::tempfile;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
  zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
  zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use crate::capture::list_displays;
use crate::error::{backend, invalid_input};

const STATE_RELEASED: u32 = 0;
const STATE_PRESSED: u32 = 1;
const BUTTON_LEFT: u32 = 0x110;
const BUTTON_RIGHT: u32 = 0x111;
const BUTTON_MIDDLE: u32 = 0x112;

#[derive(Debug)]
pub struct WaylandInputSession {
  connection: Connection,
  event_queue: EventQueue<WaylandInputState>,
  keyboard: ZwpVirtualKeyboardV1,
  pointer: ZwlrVirtualPointerV1,
  desktop_bounds: Rect,
  clock: Instant,
}

impl WaylandInputSession {
  pub fn open() -> DriverResult<Self> {
    let connection = Connection::connect_to_env().map_err(|error| backend(format!("failed to connect to Wayland compositor: {error}")))?;
    let (globals, event_queue) = registry_queue_init::<WaylandInputState>(&connection)
      .map_err(|error| backend(format!("failed to initialize Wayland input registry: {error}")))?;
    let qh = event_queue.handle();
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=9, ()).map_err(|error| backend(format!("failed to bind Wayland seat: {error}")))?;
    let keyboard_manager = globals
      .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
      .map_err(|error| backend(format!("compositor does not expose virtual keyboard v1: {error}")))?;
    let pointer_manager = globals
      .bind::<ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ())
      .map_err(|error| backend(format!("compositor does not expose wlroots virtual pointer v1: {error}")))?;
    let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
    let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
    install_keymap(&connection, &keyboard)?;
    let desktop_bounds = desktop_bounds()?;
    connection.flush().map_err(|error| backend(format!("failed to flush Wayland input setup: {error}")))?;
    Ok(Self {
      connection,
      event_queue,
      keyboard,
      pointer,
      desktop_bounds,
      clock: Instant::now(),
    })
  }

  pub fn key_press(&mut self, keysym: i32) -> DriverResult<()> {
    let key = evdev_key_for_keysym(keysym)?;
    if key.implicit_shift {
      self.send_key(KEY_LEFTSHIFT, STATE_PRESSED);
      self.send_modifiers(MOD_SHIFT);
    }
    self.send_key(key.code, STATE_PRESSED);
    self.send_key(key.code, STATE_RELEASED);
    if key.implicit_shift {
      self.send_key(KEY_LEFTSHIFT, STATE_RELEASED);
      self.send_modifiers(0);
    }
    self.flush()
  }

  pub fn key_chord(&mut self, modifiers: &[i32], keysym: i32) -> DriverResult<()> {
    let key = evdev_key_for_keysym(keysym)?;
    let mut pressed = Vec::new();
    let mut depressed_modifiers = 0;
    for keysym in modifiers {
      let modifier = evdev_key_for_keysym(*keysym)?;
      if !pressed.contains(&modifier.code) {
        self.send_key(modifier.code, STATE_PRESSED);
        pressed.push(modifier.code);
        depressed_modifiers |= modifier_mask(modifier.code);
      }
    }
    if key.implicit_shift && !pressed.contains(&KEY_LEFTSHIFT) {
      self.send_key(KEY_LEFTSHIFT, STATE_PRESSED);
      pressed.push(KEY_LEFTSHIFT);
      depressed_modifiers |= MOD_SHIFT;
    }
    if depressed_modifiers != 0 {
      self.send_modifiers(depressed_modifiers);
    }
    self.send_key(key.code, STATE_PRESSED);
    self.send_key(key.code, STATE_RELEASED);
    for code in pressed.into_iter().rev() {
      self.send_key(code, STATE_RELEASED);
    }
    if depressed_modifiers != 0 {
      self.send_modifiers(0);
    }
    self.flush()
  }

  pub fn click_at(&mut self, point: Point, click: Click) -> DriverResult<()> {
    let count = click.count();
    if count == 0 {
      return Err(invalid_input("repeated click count must be greater than zero"));
    }
    self.move_pointer_to(point)?;
    for index in 0..count {
      self.send_button(MouseButton::Left, STATE_PRESSED)?;
      self.send_button(MouseButton::Left, STATE_RELEASED)?;
      self.pointer.frame();
      self.flush()?;
      if index + 1 < count
        && let Some(interval) = click.interval()
        && !interval.is_zero()
      {
        std::thread::sleep(interval);
      }
    }
    Ok(())
  }

  pub fn scroll_at(&mut self, point: Point, scroll: Scroll) -> DriverResult<()> {
    self.move_pointer_to(point)?;
    let time = self.timestamp();
    self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
    if scroll.delta_x != 0.0 {
      self.pointer.axis(time, wl_pointer::Axis::HorizontalScroll, scroll.delta_x);
    }
    if scroll.delta_y != 0.0 {
      self.pointer.axis(time, wl_pointer::Axis::VerticalScroll, scroll.delta_y);
    }
    self.pointer.frame();
    self.flush()
  }

  fn move_pointer_to(&mut self, point: Point) -> DriverResult<()> {
    let width = self.desktop_bounds.size.width.round().max(1.0) as u32;
    let height = self.desktop_bounds.size.height.round().max(1.0) as u32;
    let x = (point.x - self.desktop_bounds.origin.x).round().clamp(0.0, f64::from(width)) as u32;
    let y = (point.y - self.desktop_bounds.origin.y).round().clamp(0.0, f64::from(height)) as u32;
    self.pointer.motion_absolute(self.timestamp(), x, y, width, height);
    self.pointer.frame();
    self.flush()
  }

  fn send_key(&self, code: u32, state: u32) {
    self.keyboard.key(self.timestamp(), code, state);
  }

  fn send_modifiers(&self, depressed: u32) {
    self.keyboard.modifiers(depressed, 0, 0, 0);
  }

  fn send_button(&self, button: MouseButton, state: u32) -> DriverResult<()> {
    let button = match button {
      MouseButton::Left => BUTTON_LEFT,
      MouseButton::Right => BUTTON_RIGHT,
      MouseButton::Middle => BUTTON_MIDDLE,
    };
    let state = match state {
      STATE_PRESSED => wl_pointer::ButtonState::Pressed,
      STATE_RELEASED => wl_pointer::ButtonState::Released,
      _ => return Err(invalid_input(format!("invalid Wayland pointer button state {state}"))),
    };
    self.pointer.button(self.timestamp(), button, state);
    Ok(())
  }

  fn flush(&mut self) -> DriverResult<()> {
    self.connection.flush().map_err(|error| backend(format!("failed to flush Wayland input event: {error}")))?;
    self
      .event_queue
      .dispatch_pending(&mut WaylandInputState)
      .map_err(|error| backend(format!("failed to dispatch Wayland input event: {error}")))?;
    Ok(())
  }

  fn timestamp(&self) -> u32 {
    self.clock.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
  }
}

fn install_keymap(connection: &Connection, keyboard: &ZwpVirtualKeyboardV1) -> DriverResult<()> {
  let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
  let keymap = xkb::Keymap::new_from_names(&context, "", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
    .ok_or_else(|| backend("failed to compile the native Wayland US keymap"))?;
  let mut bytes = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
  bytes.push(0);
  let mut file = keymap_file(&bytes)?;
  keyboard.keymap(1, file.as_fd(), bytes.len() as u32);
  file.flush().map_err(|error| backend(format!("failed to flush native Wayland keymap: {error}")))?;
  // Keep the backing fd alive until wayland-client has handed it to the
  // compositor. Dropping it before this flush makes the first key event fail
  // intermittently on a fresh session.
  connection.flush().map_err(|error| backend(format!("failed to send native Wayland keymap: {error}")))?;
  Ok(())
}

fn keymap_file(bytes: &[u8]) -> DriverResult<File> {
  let mut file = tempfile().map_err(|error| backend(format!("failed to create native Wayland keymap file: {error}")))?;
  file.write_all(bytes).map_err(|error| backend(format!("failed to write native Wayland keymap: {error}")))?;
  Ok(file)
}

fn desktop_bounds() -> DriverResult<Rect> {
  let observed = list_displays()?;
  let first = observed.displays.first().ok_or_else(|| backend("Wayland input requires at least one display"))?;
  let mut min_x = first.frame.origin.x;
  let mut min_y = first.frame.origin.y;
  let mut max_x = min_x + first.frame.size.width;
  let mut max_y = min_y + first.frame.size.height;
  for display in &observed.displays[1..] {
    let frame = display.frame;
    min_x = min_x.min(frame.origin.x);
    min_y = min_y.min(frame.origin.y);
    max_x = max_x.max(frame.origin.x + frame.size.width);
    max_y = max_y.max(frame.origin.y + frame.size.height);
  }
  Ok(Rect::new(min_x, min_y, max_x - min_x, max_y - min_y))
}

#[derive(Debug, Default)]
struct WaylandInputState;

impl Dispatch<WlRegistry, GlobalListContents> for WaylandInputState {
  fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(WaylandInputState: ignore WlSeat);
delegate_noop!(WaylandInputState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(WaylandInputState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(WaylandInputState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WaylandInputState: ignore ZwlrVirtualPointerV1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EvdevKey {
  code: u32,
  implicit_shift: bool,
}

const KEY_ESC: u32 = 1;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_SPACE: u32 = 57;
const KEY_DELETE: u32 = 111;
const KEY_LEFTMETA: u32 = 125;
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CONTROL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;

fn modifier_mask(code: u32) -> u32 {
  match code {
    KEY_LEFTSHIFT => MOD_SHIFT,
    KEY_LEFTCTRL => MOD_CONTROL,
    KEY_LEFTALT => MOD_ALT,
    KEY_LEFTMETA => MOD_SUPER,
    _ => 0,
  }
}

fn evdev_key_for_keysym(keysym: i32) -> DriverResult<EvdevKey> {
  let (code, implicit_shift) = match keysym {
    0xff1b => (KEY_ESC, false),
    0xff08 => (KEY_BACKSPACE, false),
    0xff09 => (KEY_TAB, false),
    0xff0d => (KEY_ENTER, false),
    0xffff => (KEY_DELETE, false),
    0xffe1 => (KEY_LEFTSHIFT, false),
    0xffe3 => (KEY_LEFTCTRL, false),
    0xffe9 => (KEY_LEFTALT, false),
    0xffeb => (KEY_LEFTMETA, false),
    value if value == i32::from(b' ') => (KEY_SPACE, false),
    value if (0..=0x7f).contains(&value) => ascii_evdev(value as u8)?,
    _ => return Err(invalid_input(format!("native Wayland keyboard cannot map keysym {keysym:#x}"))),
  };
  Ok(EvdevKey {
    code,
    implicit_shift,
  })
}

fn ascii_evdev(value: u8) -> DriverResult<(u32, bool)> {
  let result = match value {
    b'a'..=b'z' => (letter_key(value), false),
    b'A'..=b'Z' => (letter_key(value.to_ascii_lowercase()), true),
    b'1'..=b'9' => (u32::from(value - b'1') + 2, false),
    b'0' => (11, false),
    b'-' => (12, false),
    b'_' => (12, true),
    b'=' => (13, false),
    b'+' => (13, true),
    b'[' => (26, false),
    b'{' => (26, true),
    b']' => (27, false),
    b'}' => (27, true),
    b';' => (39, false),
    b':' => (39, true),
    b'\'' => (40, false),
    b'"' => (40, true),
    b'`' => (41, false),
    b'~' => (41, true),
    b'\\' => (43, false),
    b'|' => (43, true),
    b',' => (51, false),
    b'<' => (51, true),
    b'.' => (52, false),
    b'>' => (52, true),
    b'/' => (53, false),
    b'?' => (53, true),
    b'!' => (2, true),
    b'@' => (3, true),
    b'#' => (4, true),
    b'$' => (5, true),
    b'%' => (6, true),
    b'^' => (7, true),
    b'&' => (8, true),
    b'*' => (9, true),
    b'(' => (10, true),
    b')' => (11, true),
    _ => return Err(invalid_input(format!("native Wayland keyboard does not support ASCII byte {value:#x}"))),
  };
  Ok(result)
}

fn letter_key(value: u8) -> u32 {
  match value {
    b'q' => 16,
    b'w' => 17,
    b'e' => 18,
    b'r' => 19,
    b't' => 20,
    b'y' => 21,
    b'u' => 22,
    b'i' => 23,
    b'o' => 24,
    b'p' => 25,
    b'a' => 30,
    b's' => 31,
    b'd' => 32,
    b'f' => 33,
    b'g' => 34,
    b'h' => 35,
    b'j' => 36,
    b'k' => 37,
    b'l' => 38,
    b'z' => 44,
    b'x' => 45,
    b'c' => 46,
    b'v' => 47,
    b'b' => 48,
    b'n' => 49,
    b'm' => 50,
    _ => unreachable!("letter range checked by caller"),
  }
}

#[cfg(test)]
mod tests {
  use super::{KEY_LEFTCTRL, KEY_LEFTSHIFT, MOD_CONTROL, MOD_SHIFT, ascii_evdev, evdev_key_for_keysym, modifier_mask};

  #[test]
  fn maps_ascii_and_implicit_shift() {
    assert_eq!(ascii_evdev(b'a').unwrap(), (30, false));
    assert_eq!(ascii_evdev(b'A').unwrap(), (30, true));
    assert_eq!(ascii_evdev(b'!').unwrap(), (2, true));
  }

  #[test]
  fn maps_modifier_keysyms() {
    let shift = evdev_key_for_keysym(0xffe1).unwrap();
    assert_eq!(shift.code, KEY_LEFTSHIFT);
    assert!(!shift.implicit_shift);
  }

  #[test]
  fn maps_evdev_modifier_codes_to_xkb_masks() {
    assert_eq!(modifier_mask(KEY_LEFTSHIFT), MOD_SHIFT);
    assert_eq!(modifier_mask(KEY_LEFTCTRL), MOD_CONTROL);
    assert_eq!(modifier_mask(47), 0);
  }
}
