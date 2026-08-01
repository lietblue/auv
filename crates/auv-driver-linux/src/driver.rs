use std::sync::{Arc, Mutex};

use auv_driver_common::{Driver, DriverDescriptor, DriverResult, DriverSession};

use crate::clipboard::ClipboardSession;
use crate::descriptor::{LinuxDriverDescriptor, linux_driver_descriptor};
use crate::native::input::InputSession;
use crate::native::portal::ScreenCastSession;

#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxDriver;

impl LinuxDriver {
  pub fn new() -> Self {
    Self
  }

  pub fn linux_descriptor(&self) -> LinuxDriverDescriptor {
    linux_driver_descriptor()
  }
}

#[derive(Clone, Debug)]
pub struct LinuxDriverSession {
  pub(crate) state: Arc<Mutex<LinuxDriverSessionState>>,
}

#[derive(Debug, Default)]
pub(crate) struct LinuxDriverSessionState {
  // Portal-backed desktops still use a distinct clipboard RemoteDesktop
  // session. Niri stores a command-backed session marker here instead.
  pub(crate) clipboard_session: Option<ClipboardSession>,
  pub(crate) input_session: Option<InputSession>,
  pub(crate) screencast_session: Option<ScreenCastSession>,
}

impl LinuxDriverSession {
  pub fn linux_descriptor(&self) -> LinuxDriverDescriptor {
    linux_driver_descriptor()
  }
}

impl Driver for LinuxDriver {
  type Session = LinuxDriverSession;

  fn descriptor(&self) -> DriverDescriptor {
    self.linux_descriptor().as_driver_descriptor()
  }

  fn open_local(&self) -> DriverResult<Self::Session> {
    Ok(LinuxDriverSession {
      state: Arc::new(Mutex::new(LinuxDriverSessionState::default())),
    })
  }
}

impl DriverSession for LinuxDriverSession {
  fn descriptor(&self) -> DriverDescriptor {
    self.linux_descriptor().as_driver_descriptor()
  }
}

#[cfg(test)]
#[path = "driver_test.rs"]
mod tests;
