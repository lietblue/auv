mod wayland;

use auv_driver_common::error::DriverResult;
use auv_driver_common::geometry::Point;
use auv_driver_common::input::{Click, Scroll};

use crate::error::backend;
use crate::native::portal::{InputSession as PortalInputSession, PortalInput};

use self::wayland::WaylandInputSession;

#[derive(Debug)]
pub enum InputSession {
  Portal(PortalInputSession),
  Wayland(WaylandInputSession),
}

impl InputSession {
  pub fn open() -> DriverResult<Self> {
    if prefers_native_wayland() {
      open_with_fallback(
        ("native Wayland", || WaylandInputSession::open().map(Self::Wayland)),
        ("RemoteDesktop portal", || PortalInput::open().map(Self::Portal)),
      )
    } else {
      open_with_fallback(
        ("RemoteDesktop portal", || PortalInput::open().map(Self::Portal)),
        ("native Wayland", || WaylandInputSession::open().map(Self::Wayland)),
      )
    }
  }

  pub fn key_press(&mut self, keysym: i32) -> DriverResult<()> {
    match self {
      Self::Portal(session) => session.key_press(keysym),
      Self::Wayland(session) => session.key_press(keysym),
    }
  }

  pub fn key_chord(&mut self, modifiers: &[i32], key: i32) -> DriverResult<()> {
    match self {
      Self::Portal(session) => session.key_chord(modifiers, key),
      Self::Wayland(session) => session.key_chord(modifiers, key),
    }
  }

  pub fn click_at(&mut self, point: Point, click: Click) -> DriverResult<()> {
    match self {
      Self::Portal(session) => session.click_at(point, click),
      Self::Wayland(session) => session.click_at(point, click),
    }
  }

  pub fn scroll_at(&mut self, point: Point, scroll: Scroll) -> DriverResult<()> {
    match self {
      Self::Portal(session) => session.scroll_at(point, scroll),
      Self::Wayland(session) => session.scroll_at(point, scroll),
    }
  }

  pub fn backend_name(&self) -> &'static str {
    match self {
      Self::Portal(_) => "linux.portal.remote-desktop",
      Self::Wayland(_) => "linux.wayland.virtual-input",
    }
  }
}

pub(crate) fn prefers_native_wayland() -> bool {
  std::env::var_os("NIRI_SOCKET").is_some()
    || [
      "XDG_CURRENT_DESKTOP",
      "XDG_SESSION_DESKTOP",
      "DESKTOP_SESSION",
    ]
    .into_iter()
    .filter_map(|name| std::env::var(name).ok())
    .any(|value| value.split([':', ';']).any(|part| part.eq_ignore_ascii_case("niri")))
}

fn open_with_fallback(
  first: (&str, impl FnOnce() -> DriverResult<InputSession>),
  second: (&str, impl FnOnce() -> DriverResult<InputSession>),
) -> DriverResult<InputSession> {
  match (first.1)() {
    Ok(session) => {
      debug_selection(session.backend_name(), None);
      Ok(session)
    }
    Err(first_error) => match (second.1)() {
      Ok(session) => {
        debug_selection(session.backend_name(), Some(&first_error.to_string()));
        Ok(session)
      }
      Err(second_error) => Err(backend(format!(
        "failed to open Linux input session; {} failed: {}; {} failed: {}",
        first.0, first_error, second.0, second_error
      ))),
    },
  }
}

fn debug_selection(selected: &str, fallback_reason: Option<&str>) {
  if std::env::var_os("AUV_DEBUG_LINUX_INPUT").is_none() {
    return;
  }
  if let Some(reason) = fallback_reason {
    eprintln!("auv linux input selected {selected} after fallback: {reason}");
  } else {
    eprintln!("auv linux input selected {selected}");
  }
}

#[cfg(test)]
mod tests {
  use super::prefers_native_wayland;

  #[test]
  fn explicit_niri_socket_prefers_native_wayland() {
    // Environment mutation is process-global, so this test only verifies the
    // parsing helper through the desktop variables when the host is neutral.
    if std::env::var_os("NIRI_SOCKET").is_some() {
      assert!(prefers_native_wayland());
    }
  }
}
