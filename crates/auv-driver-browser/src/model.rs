use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use auv_driver_common::{DriverError, DriverResult, Rect, WaitOptions};
use serde::{Deserialize, Deserializer, Serialize};

/// Logical viewport dimensions for a locally launched browser.
///
/// Values are expressed in CSS pixels. The local CDP session applies the
/// dimensions with a device scale factor of `1.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserViewport {
  /// Viewport width in CSS pixels.
  pub width: u32,
  /// Viewport height in CSS pixels.
  pub height: u32,
}

impl Default for BrowserViewport {
  fn default() -> Self {
    Self {
      width: 1280,
      height: 720,
    }
  }
}

/// Configuration used by [`crate::BrowserDriver`] for local Chromium launches.
///
/// The options are validated when
/// [`auv_driver_common::Driver::open_local`] is called. Driver-owned command-line
/// flags cannot be duplicated through [`Self::extra_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserLaunchOptions {
  /// Explicit Chrome or Chromium executable.
  ///
  /// When absent, the driver checks the `CHROME` environment variable and then
  /// common platform installation paths and executable names.
  pub executable: Option<PathBuf>,
  /// Persistent Chromium user-data directory to use.
  ///
  /// When absent, the driver creates an isolated temporary profile owned by the
  /// returned session. A caller-supplied directory is created if necessary and
  /// is never deleted by the driver.
  pub user_data_dir: Option<PathBuf>,
  /// Whether to launch Chromium in its modern headless mode.
  pub headless: bool,
  /// Whether Chromium's sandbox remains enabled.
  ///
  /// Disabling this option passes `--no-sandbox` and weakens browser process
  /// isolation. It should be reserved for environments that cannot support the
  /// sandbox.
  pub sandbox: bool,
  /// Whether Chromium should ignore TLS certificate errors.
  ///
  /// The default is `false`. Enabling this weakens page-transport security and
  /// is intended only for controlled test environments.
  pub ignore_certificate_errors: bool,
  /// CSS-pixel viewport applied to each page attached by the local session.
  pub viewport: BrowserViewport,
  /// Absolute time limit for Chromium to publish its local CDP endpoint.
  pub startup_timeout: Duration,
  /// Absolute time limit for the initial CDP connection and for each later CDP
  /// command.
  ///
  /// A higher-level operation can issue several commands; navigation and DOM
  /// waits also apply their own overall deadlines.
  pub command_timeout: Duration,
  /// Additional Chromium command-line arguments.
  ///
  /// Arguments controlling debugging, profiles, viewport, headless mode,
  /// sandboxing, or certificate-error handling are owned by the typed options
  /// above and are rejected here.
  pub extra_args: Vec<OsString>,
}

impl Default for BrowserLaunchOptions {
  fn default() -> Self {
    Self {
      executable: None,
      user_data_dir: None,
      headless: true,
      sandbox: true,
      ignore_certificate_errors: false,
      viewport: BrowserViewport::default(),
      startup_timeout: Duration::from_secs(15),
      command_timeout: Duration::from_secs(30),
      extra_args: Vec::new(),
    }
  }
}

impl BrowserLaunchOptions {
  pub(crate) fn validate(&self) -> DriverResult<()> {
    if self.viewport.width == 0 || self.viewport.height == 0 {
      return Err(invalid_input("browser viewport width and height must be greater than zero"));
    }
    if self.startup_timeout.is_zero() {
      return Err(invalid_input("browser startup timeout must be greater than zero"));
    }
    if self.command_timeout.is_zero() {
      return Err(invalid_input("browser command timeout must be greater than zero"));
    }
    if let Some(executable) = &self.executable
      && !executable.is_file()
    {
      return Err(invalid_input(format!("browser executable does not exist or is not a file: {}", executable.display())));
    }
    for argument in &self.extra_args {
      let argument = argument.to_string_lossy();
      if is_driver_owned_argument(&argument) {
        return Err(invalid_input(format!(
          "browser argument {argument:?} is owned by the driver; use the corresponding typed launch option instead"
        )));
      }
    }
    Ok(())
  }
}

/// Connection settings for attaching to an existing browser CDP endpoint.
///
/// This type deliberately accepts a browser-level WebSocket endpoint rather
/// than an HTTP discovery URL. Its custom [`Debug`](fmt::Debug)
/// implementation redacts the endpoint because URLs can contain credentials or
/// other sensitive material.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserConnectOptions {
  /// Browser-level CDP WebSocket URL.
  ///
  /// Plain `ws://` is restricted to loopback hosts. Use certificate-validated
  /// `wss://` for remote endpoints.
  pub websocket_url: String,
  /// One absolute deadline covering DNS resolution and the TCP, TLS, and
  /// WebSocket handshakes.
  pub connect_timeout: Duration,
  /// Absolute time limit for each CDP command after connection.
  ///
  /// The deadline includes request serialization and socket writes as well as
  /// waiting for the matching protocol response; unrelated CDP events do not
  /// extend it. A higher-level operation can issue several commands.
  pub command_timeout: Duration,
}

impl BrowserConnectOptions {
  /// Creates connection settings with 10-second connect and 30-second command
  /// timeouts.
  pub fn new(websocket_url: impl Into<String>) -> Self {
    Self {
      websocket_url: websocket_url.into(),
      connect_timeout: Duration::from_secs(10),
      command_timeout: Duration::from_secs(30),
    }
  }

  pub(crate) fn validate(&self) -> DriverResult<()> {
    if self.websocket_url.trim().is_empty() {
      return Err(invalid_input("browser CDP WebSocket URL must not be empty"));
    }
    if self.connect_timeout.is_zero() {
      return Err(invalid_input("browser connect timeout must be greater than zero"));
    }
    if self.command_timeout.is_zero() {
      return Err(invalid_input("browser command timeout must be greater than zero"));
    }
    let url = url::Url::parse(&self.websocket_url).map_err(|error| invalid_input(format!("invalid browser CDP WebSocket URL: {error}")))?;
    if !matches!(url.scheme(), "ws" | "wss") {
      return Err(invalid_input("browser CDP endpoint must use ws:// or wss://"));
    }
    if url.scheme() == "ws" && !is_loopback_host(url.host_str()) {
      return Err(invalid_input("unencrypted browser CDP endpoints must use a loopback host; use wss:// for remote endpoints"));
    }
    Ok(())
  }
}

impl fmt::Debug for BrowserConnectOptions {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("BrowserConnectOptions")
      .field("websocket_url", &"<redacted CDP endpoint>")
      .field("connect_timeout", &self.connect_timeout)
      .field("command_timeout", &self.command_timeout)
      .finish()
  }
}

/// Opaque reference to a Chromium page target.
///
/// References are session-scoped target identifiers. A reference can become
/// invalid when the target closes and should be refreshed through
/// [`crate::PageApi::list`] when that happens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PageRef(String);

impl PageRef {
  /// Validates and wraps a non-empty Chromium target identifier.
  pub fn new(id: impl Into<String>) -> DriverResult<Self> {
    let id = id.into();
    if id.trim().is_empty() {
      return Err(invalid_input("browser page reference must not be empty"));
    }
    Ok(Self(id))
  }

  /// Returns the underlying Chromium target identifier.
  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl<'de> Deserialize<'de> for PageRef {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let id = String::deserialize(deserializer)?;
    Self::new(id).map_err(serde::de::Error::custom)
  }
}

/// Current metadata for a Chromium page target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
  /// Stable target reference used by page and DOM operations.
  pub reference: PageRef,
  /// Current URL reported by Chromium.
  pub url: String,
  /// Current page title reported by Chromium.
  pub title: String,
}

/// Document readiness state required by a navigation operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationWait {
  /// Return once the committed document reports `interactive` or `complete`.
  DomContentLoaded,
  /// Return once the committed document reports `complete`.
  #[default]
  Load,
}

/// Readiness and polling settings for page navigation and reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationOptions {
  /// Document readiness state required before returning.
  pub wait_until: NavigationWait,
  /// Overall navigation deadline and readiness polling interval.
  ///
  /// The timeout is an absolute operation deadline. Individual protocol calls
  /// use only the time remaining within it.
  pub wait: WaitOptions,
}

impl Default for NavigationOptions {
  fn default() -> Self {
    Self {
      wait_until: NavigationWait::Load,
      wait: WaitOptions::default(),
    }
  }
}

/// Options controlling a single PNG page capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageCaptureOptions {
  /// Capture the entire document when `true`, or only the visual viewport when
  /// `false`.
  ///
  /// Captures are limited to 32,768 pixels per dimension, 20 Mi pixels total,
  /// and 256 MiB of decoder allocation.
  pub full_page: bool,
}

/// Session-bound identity of a DOM element observation.
///
/// Element references are created by [`crate::DomApi`] and combine Chromium's
/// backend node identity with the document loader generation and isolated
/// execution context used for safe driver-side inspection. They are intended
/// for actions in the same live session. A navigation makes a prior reference
/// stale.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ElementRef {
  page: PageRef,
  backend_node_id: u64,
  document_loader_id: String,
  #[serde(skip)]
  execution_context_id: u64,
}

impl ElementRef {
  pub(crate) fn new(page: PageRef, backend_node_id: u64, document_loader_id: String, execution_context_id: u64) -> Self {
    Self {
      page,
      backend_node_id,
      document_loader_id,
      execution_context_id,
    }
  }

  /// Returns the page that owned the observed element.
  pub fn page(&self) -> &PageRef {
    &self.page
  }

  /// Returns Chromium's backend node identifier for the element.
  pub fn backend_node_id(&self) -> u64 {
    self.backend_node_id
  }

  /// Returns the loader identifier of the document in which the element was
  /// observed.
  pub fn document_loader_id(&self) -> &str {
    &self.document_loader_id
  }

  pub(crate) fn execution_context_id(&self) -> u64 {
    self.execution_context_id
  }
}

/// Bounded snapshot of a DOM element and its action reference.
///
/// A query returns at most 128 unindexed matches and enforces a 2 MiB aggregate
/// observation budget. Use the truncation flags rather than assuming
/// [`Self::text`] or [`Self::attributes`] are complete.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DomElement {
  /// Session-bound identity used by click and text-entry operations.
  pub reference: ElementRef,
  /// Lowercase HTML tag name observed for the element.
  pub tag_name: String,
  /// Visible text when available, falling back to text content.
  ///
  /// The value is limited to 16,384 JavaScript string units.
  pub text: String,
  /// Whether [`Self::text`] was shortened to the per-element text limit.
  pub text_truncated: bool,
  /// Bounded map of the element's attribute names and values.
  ///
  /// At most 128 attributes and 16,384 combined JavaScript string units are
  /// retained.
  pub attributes: std::collections::BTreeMap<String, String>,
  /// Whether attributes were omitted or shortened because an attribute limit
  /// was reached.
  pub attributes_truncated: bool,
  /// Element bounds relative to the current visual viewport, when the element
  /// has a non-empty client rectangle.
  pub viewport_bounds: Option<Rect>,
  /// Whether the element appeared visible from its computed style and client
  /// rectangle at observation time.
  ///
  /// This is observational metadata, not a guarantee that a later action will
  /// succeed.
  pub visible: bool,
}

/// Validated top-level CSS selector with optional match selection.
///
/// Queries use the current document's `querySelectorAll` semantics. Iframes and
/// shadow roots are not crossed by this selector contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssSelector {
  value: String,
  index: Option<usize>,
}

impl CssSelector {
  /// Validates and wraps a non-empty CSS selector string.
  ///
  /// Chromium performs the actual CSS syntax validation when the selector is
  /// queried.
  pub fn new(value: impl Into<String>) -> DriverResult<Self> {
    let value = value.into();
    if value.trim().is_empty() {
      return Err(invalid_input("CSS selector must not be empty"));
    }
    Ok(Self { value, index: None })
  }

  /// Selects one match by zero-based `querySelectorAll` index.
  ///
  /// An indexed [`crate::DomApi::query_all`] result contains either that one
  /// element or no elements when the index is out of range.
  #[must_use]
  pub fn at(mut self, index: usize) -> Self {
    self.index = Some(index);
    self
  }

  /// Returns the underlying CSS selector string.
  pub fn as_str(&self) -> &str {
    &self.value
  }

  /// Returns the selected zero-based match index, if one was specified.
  pub fn index(&self) -> Option<usize> {
    self.index
  }
}

pub(crate) fn validate_url(url: &str) -> DriverResult<()> {
  if url.trim().is_empty() {
    return Err(invalid_input("browser URL must not be empty"));
  }
  url::Url::parse(url).map_err(|error| invalid_input(format!("invalid browser URL: {error}")))?;
  Ok(())
}

pub(crate) fn validate_wait(options: WaitOptions) -> DriverResult<()> {
  if options.timeout.is_zero() {
    return Err(invalid_input("browser wait timeout must be greater than zero"));
  }
  if options.poll_interval.is_zero() {
    return Err(invalid_input("browser wait poll interval must be greater than zero"));
  }
  Ok(())
}

fn invalid_input(message: impl Into<String>) -> DriverError {
  DriverError::InvalidInput {
    message: message.into(),
  }
}

fn is_driver_owned_argument(argument: &str) -> bool {
  const OWNED_FLAGS: &[&str] = &[
    "--disable-gpu-sandbox",
    "--disable-setuid-sandbox",
    "--headless",
    "--no-sandbox",
    "--user-data-dir",
    "--window-size",
  ];

  OWNED_FLAGS.iter().any(|flag| argument == *flag || argument.strip_prefix(flag).is_some_and(|suffix| suffix.starts_with('=')))
    || argument.starts_with("--remote-debugging-")
    || argument.starts_with("--ignore-certificate-errors")
    || argument.split_once('=').map_or(argument, |(flag, _)| flag).to_ascii_lowercase().contains("sandbox")
}

fn is_loopback_host(host: Option<&str>) -> bool {
  let Some(host) = host else {
    return false;
  };
  if host.eq_ignore_ascii_case("localhost") {
    return true;
  }
  host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
#[path = "model_test.rs"]
mod tests;
