use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Child;
use std::sync::{Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use auv_driver_common::{Capture, Click, DriverError, DriverResult, InputActionResult, InputDeliveryPath, Rect, Scroll};
use base64::Engine;
use image::{GenericImageView, ImageFormat, ImageReader, Limits, RgbaImage};
use serde::Deserialize;
use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, client_tls_with_config};

use crate::model::{
  BrowserConnectOptions, BrowserViewport, CssSelector, DomElement, ElementRef, NavigationOptions, NavigationWait, Page, PageCaptureOptions,
  PageRef,
};

type CdpSocket = WebSocket<MaybeTlsStream<TcpStream>>;

const MAX_DOM_MATCHES: usize = 128;
const MAX_DOM_TEXT_CHARS: usize = 16_384;
const MAX_DOM_ATTRIBUTE_COUNT: usize = 128;
const MAX_DOM_ATTRIBUTE_CHARS: usize = 16_384;
const MAX_DOM_QUERY_BYTES: usize = 2 * 1_024 * 1_024;
const MAX_CDP_REQUEST_BYTES: usize = 1_024 * 1_024;
const MAX_CAPTURE_WIDTH: u32 = 32_768;
const MAX_CAPTURE_HEIGHT: u32 = 32_768;
const MAX_CAPTURE_PIXELS: u64 = 20 * 1_024 * 1_024;
const MAX_CAPTURE_ALLOCATION: u64 = 256 * 1_024 * 1_024;

struct CommandDeadlineWatchdog {
  cancel: Option<mpsc::SyncSender<()>>,
  worker: Option<thread::JoinHandle<bool>>,
}

impl CommandDeadlineWatchdog {
  fn start(socket: &mut CdpSocket, timeout: Duration) -> DriverResult<Self> {
    let stream =
      socket_stream(socket)?.try_clone().map_err(|error| backend(format!("failed to prepare browser CDP deadline watchdog: {error}")))?;
    let (cancel, receiver) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || match receiver.recv_timeout(timeout) {
      Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => false,
      Err(mpsc::RecvTimeoutError::Timeout) => {
        let _ = stream.shutdown(Shutdown::Both);
        true
      }
    });
    Ok(Self {
      cancel: Some(cancel),
      worker: Some(worker),
    })
  }

  fn finish(&mut self) -> bool {
    if let Some(cancel) = self.cancel.take() {
      let _ = cancel.try_send(());
    }
    self.worker.take().is_some_and(|worker| worker.join().unwrap_or(true))
  }
}

impl Drop for CommandDeadlineWatchdog {
  fn drop(&mut self) {
    let _ = self.finish();
  }
}

pub(crate) struct OwnedBrowserProcess {
  child: Child,
  _profile: Option<tempfile::TempDir>,
}

impl OwnedBrowserProcess {
  pub(crate) fn new(child: Child, profile: Option<tempfile::TempDir>) -> Self {
    Self {
      child,
      _profile: profile,
    }
  }
}

impl Drop for OwnedBrowserProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

pub(crate) struct CdpBackend {
  connection: Mutex<CdpConnection>,
  _process: Option<OwnedBrowserProcess>,
}

impl Drop for CdpBackend {
  fn drop(&mut self) {
    if self._process.is_none() {
      return;
    }
    if let Ok(connection) = self.connection.get_mut() {
      let timeout = connection.command_timeout.min(Duration::from_secs(1));
      let _ = connection.call_with_timeout("Browser.close", json!({}), None, timeout);
    }
  }
}

impl CdpBackend {
  pub(crate) fn connect(
    options: &BrowserConnectOptions,
    process: Option<OwnedBrowserProcess>,
    viewport: Option<BrowserViewport>,
  ) -> DriverResult<Self> {
    options.validate()?;
    let socket_config = WebSocketConfig::default().max_message_size(Some(64 << 20)).max_frame_size(Some(16 << 20));
    let mut socket = connect_cdp_socket(options, socket_config)?;
    configure_socket_timeout(&mut socket, options.command_timeout)?;
    Ok(Self {
      connection: Mutex::new(CdpConnection {
        socket,
        next_id: 1,
        page_sessions: HashMap::new(),
        viewport,
        command_timeout: options.command_timeout,
        operation_deadline: None,
        tainted: false,
      }),
      _process: process,
    })
  }

  pub(crate) fn list_pages(&self) -> DriverResult<Vec<Page>> {
    self.connection()?.list_pages()
  }

  pub(crate) fn resolve_page(&self, page: &PageRef) -> DriverResult<Page> {
    self.connection()?.resolve_page(page)
  }

  pub(crate) fn open_page(&self, url: &str, options: NavigationOptions) -> DriverResult<Page> {
    let mut connection = self.connection()?;
    let result = connection.call("Target.createTarget", json!({ "url": "about:blank" }), None)?;
    let page = PageRef::new(required_str(&result, "targetId", "Target.createTarget")?)?;
    match connection.navigate(&page, url, options) {
      Ok(page) => Ok(page),
      Err(error) => {
        let _ = connection.close_page(&page);
        Err(error)
      }
    }
  }

  pub(crate) fn close_page(&self, page: &PageRef) -> DriverResult<()> {
    self.connection()?.close_page(page)
  }

  pub(crate) fn navigate(&self, page: &PageRef, url: &str, options: NavigationOptions) -> DriverResult<Page> {
    self.connection()?.navigate(page, url, options)
  }

  pub(crate) fn reload(&self, page: &PageRef, options: NavigationOptions) -> DriverResult<Page> {
    self.connection()?.reload(page, options)
  }

  pub(crate) fn capture(&self, page: &PageRef, options: PageCaptureOptions) -> DriverResult<Capture> {
    self.connection()?.capture(page, options)
  }

  pub(crate) fn evaluate_json(&self, page: &PageRef, expression: &str) -> DriverResult<Value> {
    self.connection()?.evaluate_json(page, expression)
  }

  pub(crate) fn query_all(&self, page: &PageRef, selector: &CssSelector) -> DriverResult<Vec<DomElement>> {
    self.connection()?.query_all(page, selector)
  }

  pub(crate) fn query_all_with_timeout(&self, page: &PageRef, selector: &CssSelector, timeout: Duration) -> DriverResult<Vec<DomElement>> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| backend("browser operation deadline overflowed"))?;
    self.connection_before(deadline)?.with_operation_deadline(deadline, |connection| connection.query_all(page, selector))
  }

  pub(crate) fn click(&self, element: &DomElement, click: Click) -> DriverResult<InputActionResult> {
    self.connection()?.click(element, click)
  }

  pub(crate) fn type_text(&self, element: &DomElement, text: &str) -> DriverResult<InputActionResult> {
    self.connection()?.type_text(element, text)
  }

  pub(crate) fn scroll(&self, page: &PageRef, scroll: Scroll) -> DriverResult<InputActionResult> {
    self.connection()?.scroll(page, scroll)
  }

  fn connection(&self) -> DriverResult<std::sync::MutexGuard<'_, CdpConnection>> {
    self.connection.lock().map_err(|_| backend("browser CDP connection lock was poisoned"))
  }

  fn connection_before(&self, deadline: Instant) -> DriverResult<std::sync::MutexGuard<'_, CdpConnection>> {
    loop {
      match self.connection.try_lock() {
        Ok(connection) => return Ok(connection),
        Err(TryLockError::Poisoned(_)) => return Err(backend("browser CDP connection lock was poisoned")),
        Err(TryLockError::WouldBlock) => {
          let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| backend("browser operation deadline expired while waiting for the CDP connection"))?;
          thread::sleep(Duration::from_millis(1).min(remaining));
        }
      }
    }
  }
}

struct CdpConnection {
  socket: CdpSocket,
  next_id: u64,
  page_sessions: HashMap<PageRef, String>,
  viewport: Option<BrowserViewport>,
  command_timeout: Duration,
  operation_deadline: Option<Instant>,
  tainted: bool,
}

impl CdpConnection {
  fn with_operation_deadline<T>(&mut self, deadline: Instant, operation: impl FnOnce(&mut Self) -> DriverResult<T>) -> DriverResult<T> {
    let previous = self.operation_deadline;
    self.operation_deadline = Some(previous.map_or(deadline, |existing| existing.min(deadline)));
    let result = operation(self);
    self.operation_deadline = previous;
    result
  }

  fn list_pages(&mut self) -> DriverResult<Vec<Page>> {
    let result = self.call("Target.getTargets", json!({}), None)?;
    let targets = result
      .get("targetInfos")
      .and_then(Value::as_array)
      .ok_or_else(|| backend("Target.getTargets response did not include targetInfos"))?;
    targets.iter().filter(|target| target.get("type").and_then(Value::as_str) == Some("page")).map(page_from_target).collect()
  }

  fn resolve_page(&mut self, page: &PageRef) -> DriverResult<Page> {
    self.list_pages()?.into_iter().find(|candidate| candidate.reference == *page).ok_or_else(|| DriverError::NotFound {
      target: format!("browser page {}", page.as_str()),
    })
  }

  fn close_page(&mut self, page: &PageRef) -> DriverResult<()> {
    self.resolve_page(page)?;
    let result = self.call("Target.closeTarget", json!({ "targetId": page.as_str() }), None)?;
    if result.get("success").and_then(Value::as_bool) == Some(false) {
      return Err(backend(format!("browser refused to close page {}", page.as_str())));
    }
    self.page_sessions.remove(page);
    Ok(())
  }

  fn navigate(&mut self, page: &PageRef, url: &str, options: NavigationOptions) -> DriverResult<Page> {
    self.resolve_page(page)?;
    let result = self.call_page(page, "Page.navigate", json!({ "url": url }))?;
    if let Some(error_text) = result.get("errorText").and_then(Value::as_str) {
      return Err(backend(format!("browser navigation failed: {error_text}")));
    }
    if result.get("isDownload").and_then(Value::as_bool) == Some(true) {
      return Err(DriverError::Unsupported {
        operation: "browser navigation that starts a download",
      });
    }
    let frame_id = required_str(&result, "frameId", "Page.navigate")?;
    let loader_id = result.get("loaderId").and_then(Value::as_str);
    self.wait_for_navigation(page, options, frame_id, loader_id, None)?;
    self.resolve_page(page)
  }

  fn reload(&mut self, page: &PageRef, options: NavigationOptions) -> DriverResult<Page> {
    self.resolve_page(page)?;
    let previous_frame = self.root_frame(page)?;
    self.call_page(page, "Page.reload", json!({ "ignoreCache": false }))?;
    self.wait_for_navigation(page, options, &previous_frame.frame_id, None, Some(&previous_frame.loader_id))?;
    self.resolve_page(page)
  }

  fn wait_for_navigation(
    &mut self,
    page: &PageRef,
    options: NavigationOptions,
    expected_frame_id: &str,
    expected_loader_id: Option<&str>,
    previous_loader_id: Option<&str>,
  ) -> DriverResult<()> {
    let deadline = Instant::now().checked_add(options.wait.timeout).ok_or_else(|| backend("browser navigation wait deadline overflowed"))?;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| backend("browser navigation did not reach the requested readiness state before timeout"))?;
      let frame = self.root_frame_with_timeout(page, remaining)?;
      let committed = frame.frame_id == expected_frame_id
        && expected_loader_id.is_none_or(|loader_id| frame.loader_id == loader_id)
        && previous_loader_id.is_none_or(|loader_id| frame.loader_id != loader_id);
      if committed {
        let remaining = deadline
          .checked_duration_since(Instant::now())
          .ok_or_else(|| backend("browser navigation did not reach the requested readiness state before timeout"))?;
        let ready_state = self.ready_state_with_timeout(page, &frame.frame_id, remaining)?;
        let ready = match options.wait_until {
          NavigationWait::DomContentLoaded => matches!(ready_state.as_str(), Some("interactive" | "complete")),
          NavigationWait::Load => ready_state.as_str() == Some("complete"),
        };
        if ready {
          let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| backend("browser navigation did not reach the requested readiness state before timeout"))?;
          let confirmed_frame = self.root_frame_with_timeout(page, remaining)?;
          let still_committed = confirmed_frame.frame_id == expected_frame_id
            && expected_loader_id.is_none_or(|loader_id| confirmed_frame.loader_id == loader_id)
            && previous_loader_id.is_none_or(|loader_id| confirmed_frame.loader_id != loader_id);
          if still_committed {
            return Ok(());
          }
        }
      }
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| backend("browser navigation did not reach the requested readiness state before timeout"))?;
      thread::sleep(options.wait.poll_interval.min(remaining));
    }
  }

  fn capture(&mut self, page: &PageRef, options: PageCaptureOptions) -> DriverResult<Capture> {
    self.resolve_page(page)?;
    let metrics = self.call_page(page, "Page.getLayoutMetrics", json!({}))?;
    let size = if options.full_page {
      metrics.get("cssContentSize").or_else(|| metrics.get("contentSize"))
    } else {
      metrics.get("cssVisualViewport").or_else(|| metrics.get("visualViewport"))
    }
    .ok_or_else(|| backend("Page.getLayoutMetrics response did not include the requested capture size"))?;
    let logical_width = required_f64(
      size,
      if options.full_page {
        "width"
      } else {
        "clientWidth"
      },
      "Page.getLayoutMetrics",
    )?;
    let logical_height = required_f64(
      size,
      if options.full_page {
        "height"
      } else {
        "clientHeight"
      },
      "Page.getLayoutMetrics",
    )?;
    validate_capture_dimensions(logical_width, logical_height)?;
    let mut params = json!({
      "format": "png",
      "fromSurface": true,
      "captureBeyondViewport": options.full_page,
    });
    if options.full_page {
      params["clip"] = json!({
        "x": 0.0,
        "y": 0.0,
        "width": logical_width,
        "height": logical_height,
        "scale": 1.0,
      });
    }
    let result = self.call_page(page, "Page.captureScreenshot", params)?;
    let encoded = required_str(&result, "data", "Page.captureScreenshot")?;
    let image = decode_capture_png(encoded)?;
    let (width, height) = image.dimensions();
    let scale_x = f64::from(width) / logical_width;
    let scale_y = f64::from(height) / logical_height;
    if (scale_x - scale_y).abs() > scale_x.max(scale_y) * 0.02 {
      return Err(backend("browser screenshot pixel dimensions did not match its logical page bounds"));
    }
    let scale_factor = (scale_x + scale_y) / 2.0;
    // TODO(browser-cdp-screencast): continuous frame streaming and recording
    // are intentionally outside this page-capture slice; add them only when an
    // owner-approved media contract defines timestamps and artifact ownership.
    Ok(Capture {
      image,
      bounds: Rect::new(0.0, 0.0, logical_width, logical_height),
      scale_factor,
      backend: "browser.cdp".to_string(),
      fallback_reason: None,
    })
  }

  fn evaluate_json(&mut self, page: &PageRef, expression: &str) -> DriverResult<Value> {
    self.resolve_page(page)?;
    let timeout_millis = u64::try_from(self.command_timeout.as_millis()).unwrap_or(u64::MAX);
    let result = self.call_page(
      page,
      "Runtime.evaluate",
      json!({
        "expression": expression,
        "awaitPromise": true,
        "returnByValue": true,
        "userGesture": false,
        "timeout": timeout_millis,
      }),
    )?;
    if let Some(exception) = result.get("exceptionDetails") {
      let detail = exception
        .get("exception")
        .and_then(|value| value.get("description"))
        .and_then(Value::as_str)
        .or_else(|| exception.get("text").and_then(Value::as_str))
        .unwrap_or("unknown JavaScript exception");
      return Err(DriverError::InvalidInput {
        message: format!("browser JavaScript evaluation failed: {detail}"),
      });
    }
    let remote = result.get("result").ok_or_else(|| backend("Runtime.evaluate response did not include result"))?;
    if let Some(value) = remote.get("value") {
      return Ok(value.clone());
    }
    let value_type = remote.get("type").and_then(Value::as_str).unwrap_or("unknown");
    let detail = remote.get("unserializableValue").and_then(Value::as_str).unwrap_or(value_type);
    Err(DriverError::InvalidInput {
      message: format!("browser JavaScript result is not JSON-serializable: {detail}"),
    })
  }

  fn query_all(&mut self, page: &PageRef, selector: &CssSelector) -> DriverResult<Vec<DomElement>> {
    // TODO(browser-dom-boundaries): iframe, shadow-DOM, and accessibility
    // selectors need explicit cross-document identity semantics before they can
    // be added to this strict top-level CSS selector contract.
    self.resolve_page(page)?;
    let document = self.root_frame(page)?;
    let execution_context_id = self.isolated_world_context(page, &document.frame_id)?;
    self.call_page(page, "DOM.enable", json!({}))?;
    let root = self.call_page(page, "DOM.getDocument", json!({ "depth": 0, "pierce": false }))?;
    let root_id = root
      .get("root")
      .and_then(|root| root.get("nodeId"))
      .and_then(Value::as_u64)
      .ok_or_else(|| backend("DOM.getDocument response did not include root.nodeId"))?;
    let nodes = self.call_page(
      page,
      "DOM.querySelectorAll",
      json!({
        "nodeId": root_id,
        "selector": selector.as_str(),
      }),
    )?;
    let node_values =
      nodes.get("nodeIds").and_then(Value::as_array).ok_or_else(|| backend("DOM.querySelectorAll response did not include nodeIds"))?;
    if selector.index().is_none() && node_values.len() > MAX_DOM_MATCHES {
      return Err(DriverError::InvalidInput {
        message: format!(
          "CSS selector {:?} matched more than the browser observation limit of {MAX_DOM_MATCHES} elements; narrow the selector or choose an explicit index",
          selector.as_str()
        ),
      });
    }
    let selected_values: Vec<&Value> = match selector.index() {
      Some(index) => node_values.get(index).into_iter().collect(),
      None => node_values.iter().collect(),
    };
    let node_ids = selected_values
      .into_iter()
      .map(|value| value.as_u64().ok_or_else(|| backend("DOM.querySelectorAll returned a non-integer node id")))
      .collect::<DriverResult<Vec<_>>>()?;

    let mut elements = Vec::with_capacity(node_ids.len());
    let mut observation_bytes = 0usize;
    for node_id in node_ids {
      let described = self.call_page(
        page,
        "DOM.describeNode",
        json!({
          "nodeId": node_id,
          "depth": 0,
          "pierce": false,
        }),
      )?;
      let backend_node_id = described
        .get("node")
        .and_then(|node| node.get("backendNodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| backend("DOM.describeNode response did not include node.backendNodeId"))?;
      let reference = ElementRef::new(page.clone(), backend_node_id, document.loader_id.clone(), execution_context_id);
      let snapshot = self.snapshot_element(&reference)?;
      observation_bytes = observation_bytes
        .checked_add(dom_snapshot_bytes(&snapshot))
        .filter(|bytes| *bytes <= MAX_DOM_QUERY_BYTES)
        .ok_or_else(|| DriverError::InvalidInput {
          message: format!(
            "CSS selector {:?} exceeded the {MAX_DOM_QUERY_BYTES}-byte browser observation limit; narrow the selector or choose an explicit index",
            selector.as_str()
          ),
        })?;
      elements.push(DomElement {
        reference,
        tag_name: snapshot.tag_name,
        text: snapshot.text,
        text_truncated: snapshot.text_truncated,
        attributes: snapshot.attributes,
        attributes_truncated: snapshot.attributes_truncated,
        viewport_bounds: snapshot.viewport_bounds,
        visible: snapshot.visible,
      });
    }
    if self.root_frame(page)?.loader_id != document.loader_id {
      return Err(DriverError::StaleObservation {
        message: "browser document changed while resolving the CSS selector".to_string(),
        recovery: Some("query the selector again and retry the action".to_string()),
      });
    }
    Ok(elements)
  }

  fn click(&mut self, element: &DomElement, click: Click) -> DriverResult<InputActionResult> {
    self.ensure_element_current(&element.reference)?;
    self
      .call_page(
        element.reference.page(),
        "DOM.scrollIntoViewIfNeeded",
        json!({
          "backendNodeId": element.reference.backend_node_id(),
        }),
      )
      .map_err(|error| stale_element(&element.reference, error))?;
    let (hover_x, hover_y) = self.element_center(&element.reference)?;
    self.call_page(
      element.reference.page(),
      "Input.dispatchMouseEvent",
      json!({
        "type": "mouseMoved",
        "x": hover_x,
        "y": hover_y,
      }),
    )?;
    self.ensure_element_current(&element.reference)?;
    let (x, y) = self.element_center(&element.reference)?;
    if !self.element_receives_pointer(&element.reference, x, y)? {
      return Err(DriverError::StaleObservation {
        message: format!(
          "browser element {} on page {} is obscured at its click point after hover",
          element.reference.backend_node_id(),
          element.reference.page().as_str()
        ),
        recovery: Some("query the element again after the page settles, then retry the action".to_string()),
      });
    }
    match click {
      Click::Single => self.dispatch_click_pair(element.reference.page(), x, y, 1)?,
      Click::Double { interval } => {
        self.dispatch_click_pair(element.reference.page(), x, y, 1)?;
        thread::sleep(interval);
        self.ensure_element_current(&element.reference)?;
        let (second_hover_x, second_hover_y) = self.element_center(&element.reference)?;
        self.call_page(
          element.reference.page(),
          "Input.dispatchMouseEvent",
          json!({
            "type": "mouseMoved",
            "x": second_hover_x,
            "y": second_hover_y,
          }),
        )?;
        self.ensure_element_current(&element.reference)?;
        let (second_x, second_y) = self.element_center(&element.reference)?;
        if !self.element_receives_pointer(&element.reference, second_x, second_y)? {
          return Err(DriverError::StaleObservation {
            message: format!(
              "browser element {} on page {} moved behind another hit target between double-click events",
              element.reference.backend_node_id(),
              element.reference.page().as_str()
            ),
            recovery: Some("query the element again after the page settles, then retry the action".to_string()),
          });
        }
        self.dispatch_click_pair(element.reference.page(), second_x, second_y, 2)?;
      }
    }
    Ok(InputActionResult::single_success(InputDeliveryPath::CdpInput))
  }

  fn type_text(&mut self, element: &DomElement, text: &str) -> DriverResult<InputActionResult> {
    self.ensure_element_current(&element.reference)?;
    self
      .call_page(
        element.reference.page(),
        "DOM.focus",
        json!({
          "backendNodeId": element.reference.backend_node_id(),
        }),
      )
      .map_err(|error| stale_element(&element.reference, error))?;
    self.ensure_element_current(&element.reference)?;
    if !self.element_has_focus(&element.reference)? {
      return Err(DriverError::StaleObservation {
        message: format!(
          "browser element {} on page {} did not retain focus for text input",
          element.reference.backend_node_id(),
          element.reference.page().as_str()
        ),
        recovery: Some("query and focus the element again after the page settles".to_string()),
      });
    }
    self.call_page(
      element.reference.page(),
      "Input.insertText",
      json!({
        "text": text,
      }),
    )?;
    Ok(InputActionResult::single_success(InputDeliveryPath::CdpInput))
  }

  fn scroll(&mut self, page: &PageRef, scroll: Scroll) -> DriverResult<InputActionResult> {
    self.resolve_page(page)?;
    let metrics = self.call_page(page, "Page.getLayoutMetrics", json!({}))?;
    let viewport = metrics
      .get("cssVisualViewport")
      .or_else(|| metrics.get("visualViewport"))
      .ok_or_else(|| backend("Page.getLayoutMetrics response did not include the visual viewport"))?;
    let x = required_f64(viewport, "clientWidth", "Page.getLayoutMetrics")? / 2.0;
    let y = required_f64(viewport, "clientHeight", "Page.getLayoutMetrics")? / 2.0;
    self.call_page(
      page,
      "Input.dispatchMouseEvent",
      json!({
        "type": "mouseWheel",
        "x": x,
        "y": y,
        "deltaX": scroll.delta_x,
        "deltaY": scroll.delta_y,
      }),
    )?;
    Ok(InputActionResult::single_success(InputDeliveryPath::CdpInput))
  }

  fn element_center(&mut self, element: &ElementRef) -> DriverResult<(f64, f64)> {
    let result = self
      .call_page(
        element.page(),
        "DOM.getBoxModel",
        json!({
          "backendNodeId": element.backend_node_id(),
        }),
      )
      .map_err(|error| stale_element(element, error))?;
    let points = result
      .get("model")
      .and_then(|model| model.get("content"))
      .and_then(Value::as_array)
      .ok_or_else(|| stale_element(element, backend("DOM.getBoxModel response did not include model.content")))?;
    if points.len() != 8 {
      return Err(stale_element(element, backend("DOM.getBoxModel returned a content quad with an unexpected point count")));
    }
    let values = points
      .iter()
      .map(|value| value.as_f64().ok_or_else(|| backend("DOM.getBoxModel returned a non-numeric content point")))
      .collect::<DriverResult<Vec<_>>>()?;
    let xs = [values[0], values[2], values[4], values[6]];
    let ys = [values[1], values[3], values[5], values[7]];
    let x = (xs.iter().copied().fold(f64::INFINITY, f64::min) + xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)) / 2.0;
    let y = (ys.iter().copied().fold(f64::INFINITY, f64::min) + ys.iter().copied().fold(f64::NEG_INFINITY, f64::max)) / 2.0;
    Ok((x, y))
  }

  fn snapshot_element(&mut self, element: &ElementRef) -> DriverResult<DomSnapshot> {
    let object_id = self.resolve_object_id(element)?;
    let snapshot = self.call_page(
      element.page(),
      "Runtime.callFunctionOn",
      json!({
        "objectId": object_id,
        "functionDeclaration": r#"function(maxTextChars, maxAttributeCount, maxAttributeChars) {
          const rect = this.getBoundingClientRect();
          const style = this.ownerDocument.defaultView.getComputedStyle(this);
          const finiteBounds = [rect.x, rect.y, rect.width, rect.height].every(Number.isFinite);
          const rawText = typeof this.innerText === "string" ? this.innerText : (this.textContent || "");
          const text = rawText.slice(0, maxTextChars);
          const rawAttributes = Array.from(this.attributes);
          const attributePairs = [];
          let attributeChars = 0;
          let attributesTruncated = rawAttributes.length > maxAttributeCount;
          for (const attribute of rawAttributes.slice(0, maxAttributeCount)) {
            let remaining = Math.max(0, maxAttributeChars - attributeChars);
            if (remaining === 0) {
              attributesTruncated = true;
              break;
            }
            const name = attribute.name.slice(0, remaining);
            attributeChars += name.length;
            remaining = Math.max(0, maxAttributeChars - attributeChars);
            const value = attribute.value.slice(0, remaining);
            attributeChars += value.length;
            if (name.length !== attribute.name.length || value.length !== attribute.value.length) {
              attributesTruncated = true;
            }
            attributePairs.push([name, value]);
          }
          return {
            tag_name: this.tagName.toLowerCase(),
            text,
            text_truncated: rawText.length > text.length,
            attributes: Object.fromEntries(attributePairs),
            attributes_truncated: attributesTruncated,
            viewport_bounds: finiteBounds ? {
              origin: { x: rect.x, y: rect.y },
              size: { width: rect.width, height: rect.height }
            } : null,
            visible: rect.width > 0 && rect.height > 0 && style.display !== "none" && style.visibility !== "hidden"
          };
        }"#,
        "arguments": [
          { "value": MAX_DOM_TEXT_CHARS },
          { "value": MAX_DOM_ATTRIBUTE_COUNT },
          { "value": MAX_DOM_ATTRIBUTE_CHARS },
        ],
        "awaitPromise": false,
        "returnByValue": true,
      }),
    );
    let _ = self.release_object(element.page(), &object_id);
    let result = snapshot.map_err(|error| stale_element(element, error))?;
    let value = runtime_value(&result, "browser DOM snapshot").map_err(|error| stale_element(element, error))?;
    serde_json::from_value(value)
      .map_err(|error| stale_element(element, backend(format!("browser DOM snapshot had an unexpected shape: {error}"))))
  }

  fn element_receives_pointer(&mut self, element: &ElementRef, x: f64, y: f64) -> DriverResult<bool> {
    let object_id = self.resolve_object_id(element)?;
    let hit_test = self.call_page(
      element.page(),
      "Runtime.callFunctionOn",
      json!({
        "objectId": object_id,
        "functionDeclaration": r#"function(x, y) {
          const hit = this.ownerDocument.elementFromPoint(x, y);
          return Boolean(hit && (hit === this || this.contains(hit)));
        }"#,
        "arguments": [{ "value": x }, { "value": y }],
        "awaitPromise": false,
        "returnByValue": true,
      }),
    );
    let _ = self.release_object(element.page(), &object_id);
    let result = hit_test.map_err(|error| stale_element(element, error))?;
    runtime_value(&result, "browser element hit test")?
      .as_bool()
      .ok_or_else(|| stale_element(element, backend("browser element hit test did not return a boolean")))
  }

  fn element_has_focus(&mut self, element: &ElementRef) -> DriverResult<bool> {
    let object_id = self.resolve_object_id(element)?;
    let focus_test = self.call_page(
      element.page(),
      "Runtime.callFunctionOn",
      json!({
        "objectId": object_id,
        "functionDeclaration": r#"function() {
          const active = this.ownerDocument.activeElement;
          return Boolean(active && (active === this || this.contains(active)));
        }"#,
        "awaitPromise": false,
        "returnByValue": true,
      }),
    );
    let _ = self.release_object(element.page(), &object_id);
    let result = focus_test.map_err(|error| stale_element(element, error))?;
    runtime_value(&result, "browser element focus test")?
      .as_bool()
      .ok_or_else(|| stale_element(element, backend("browser element focus test did not return a boolean")))
  }

  fn resolve_object_id(&mut self, element: &ElementRef) -> DriverResult<String> {
    let result = self
      .call_page(
        element.page(),
        "DOM.resolveNode",
        json!({
          "backendNodeId": element.backend_node_id(),
          "executionContextId": element.execution_context_id(),
        }),
      )
      .map_err(|error| stale_element(element, error))?;
    result
      .get("object")
      .and_then(|object| object.get("objectId"))
      .and_then(Value::as_str)
      .map(str::to_string)
      .ok_or_else(|| stale_element(element, backend("DOM.resolveNode response did not include object.objectId")))
  }

  fn release_object(&mut self, page: &PageRef, object_id: &str) -> DriverResult<()> {
    self.call_page(page, "Runtime.releaseObject", json!({ "objectId": object_id }))?;
    Ok(())
  }

  fn ensure_element_current(&mut self, element: &ElementRef) -> DriverResult<()> {
    let frame = self.root_frame(element.page())?;
    if frame.loader_id != element.document_loader_id() {
      return Err(DriverError::StaleObservation {
        message: format!("browser element {} belongs to an earlier document on page {}", element.backend_node_id(), element.page().as_str()),
        recovery: Some("query the element again after navigation before retrying the action".to_string()),
      });
    }
    Ok(())
  }

  fn dispatch_click_pair(&mut self, page: &PageRef, x: f64, y: f64, click_count: u8) -> DriverResult<()> {
    self.call_page(
      page,
      "Input.dispatchMouseEvent",
      json!({
        "type": "mousePressed",
        "x": x,
        "y": y,
        "button": "left",
        "buttons": 1,
        "clickCount": click_count,
      }),
    )?;
    let release = self.call_page(
      page,
      "Input.dispatchMouseEvent",
      json!({
        "type": "mouseReleased",
        "x": x,
        "y": y,
        "button": "left",
        "buttons": 0,
        "clickCount": click_count,
      }),
    );
    if let Err(error) = release {
      let _ = self.call_page(
        page,
        "Input.dispatchMouseEvent",
        json!({
          "type": "mouseReleased",
          "x": x,
          "y": y,
          "button": "left",
          "buttons": 0,
          "clickCount": click_count,
        }),
      );
      return Err(error);
    }
    Ok(())
  }

  fn call_page(&mut self, page: &PageRef, method: &str, params: Value) -> DriverResult<Value> {
    let session = self.page_session(page)?;
    self.call(method, params, Some(&session))
  }

  fn page_session(&mut self, page: &PageRef) -> DriverResult<String> {
    if let Some(session) = self.page_sessions.get(page) {
      return Ok(session.clone());
    }
    self.resolve_page(page)?;
    let result = self.call(
      "Target.attachToTarget",
      json!({
        "targetId": page.as_str(),
        "flatten": true,
      }),
      None,
    )?;
    let session = required_str(&result, "sessionId", "Target.attachToTarget")?.to_string();
    let initialization: DriverResult<()> = (|| {
      self.call("Page.enable", json!({}), Some(&session))?;
      self.call("Runtime.enable", json!({}), Some(&session))?;
      if let Some(viewport) = self.viewport {
        self.call(
          "Emulation.setDeviceMetricsOverride",
          json!({
            "width": viewport.width,
            "height": viewport.height,
            "deviceScaleFactor": 1.0,
            "mobile": false,
          }),
          Some(&session),
        )?;
      }
      Ok(())
    })();
    if let Err(error) = initialization {
      let _ = self.call("Target.detachFromTarget", json!({ "sessionId": session }), None);
      return Err(error);
    }
    self.page_sessions.insert(page.clone(), session.clone());
    Ok(session)
  }

  fn root_frame(&mut self, page: &PageRef) -> DriverResult<FrameIdentity> {
    let result = self.call_page(page, "Page.getFrameTree", json!({}))?;
    frame_identity(&result)
  }

  fn root_frame_with_timeout(&mut self, page: &PageRef, timeout: Duration) -> DriverResult<FrameIdentity> {
    let session = self.page_session(page)?;
    let result = self.call_with_timeout("Page.getFrameTree", json!({}), Some(&session), timeout)?;
    frame_identity(&result)
  }

  fn ready_state_with_timeout(&mut self, page: &PageRef, frame_id: &str, timeout: Duration) -> DriverResult<Value> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| backend("browser readiness deadline overflowed"))?;
    let session = self.page_session(page)?;
    let context = self.call_with_timeout(
      "Page.createIsolatedWorld",
      json!({
        "frameId": frame_id,
        "worldName": "auv-driver-browser-readiness",
        "grantUniveralAccess": false,
      }),
      Some(&session),
      timeout,
    )?;
    let context_id = context
      .get("executionContextId")
      .and_then(Value::as_u64)
      .ok_or_else(|| backend("Page.createIsolatedWorld response did not include executionContextId"))?;
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .filter(|duration| !duration.is_zero())
      .ok_or_else(|| backend("browser navigation readiness evaluation exceeded its deadline"))?;
    let timeout_millis = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    let result = self.call_with_timeout(
      "Runtime.evaluate",
      json!({
        "expression": "document.readyState",
        "contextId": context_id,
        "awaitPromise": true,
        "returnByValue": true,
        "userGesture": false,
        "timeout": timeout_millis,
      }),
      Some(&session),
      remaining,
    )?;
    runtime_value(&result, "browser document readiness evaluation")
  }

  fn isolated_world_context(&mut self, page: &PageRef, frame_id: &str) -> DriverResult<u64> {
    let result = self.call_page(
      page,
      "Page.createIsolatedWorld",
      json!({
        "frameId": frame_id,
        "worldName": "auv-driver-browser",
        "grantUniveralAccess": false,
      }),
    )?;
    result
      .get("executionContextId")
      .and_then(Value::as_u64)
      .ok_or_else(|| backend("Page.createIsolatedWorld response did not include executionContextId"))
  }

  fn call(&mut self, method: &str, params: Value, session_id: Option<&str>) -> DriverResult<Value> {
    let timeout = match self.operation_deadline {
      Some(deadline) => deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| backend(format!("browser operation deadline expired before {method}")))?
        .min(self.command_timeout),
      None => self.command_timeout,
    };
    self.call_with_timeout(method, params, session_id, timeout)
  }

  fn call_with_timeout(&mut self, method: &str, params: Value, session_id: Option<&str>, timeout: Duration) -> DriverResult<Value> {
    if self.tainted {
      return Err(backend("browser CDP connection is no longer reusable after an earlier transport failure"));
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| backend("browser CDP command deadline overflowed"))?;
    let id = self.next_id;
    self.next_id = self.next_id.checked_add(1).ok_or_else(|| backend("browser CDP request id overflowed"))?;
    let mut request = json!({
      "id": id,
      "method": method,
      "params": params,
    });
    if let Some(session_id) = session_id {
      request["sessionId"] = Value::String(session_id.to_string());
    }
    let request = serde_json::to_string(&request).map_err(|error| backend(format!("failed to encode browser CDP request: {error}")))?;
    if request.len() > MAX_CDP_REQUEST_BYTES {
      return Err(DriverError::InvalidInput {
        message: format!("browser CDP request {method} exceeded the {MAX_CDP_REQUEST_BYTES}-byte request limit"),
      });
    }
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .filter(|duration| !duration.is_zero())
      .ok_or_else(|| backend(format!("browser CDP command {method} exceeded its deadline before it was sent")))?;
    let mut watchdog = match CommandDeadlineWatchdog::start(&mut self.socket, remaining) {
      Ok(watchdog) => watchdog,
      Err(error) => return Err(self.taint(format!("failed to start browser CDP deadline watchdog for {method}: {error}"))),
    };
    let result = (|| {
      if let Err(error) = configure_socket_write_timeout(&mut self.socket, remaining) {
        return Err(self.taint(format!("failed to update browser CDP send deadline for {method}: {error}")));
      }
      if let Err(error) = self.socket.send(Message::Text(request.into())) {
        return Err(self.taint(format!("failed to send browser CDP request {method}: {error}; command outcome is unknown")));
      }

      loop {
        let remaining = deadline
          .checked_duration_since(Instant::now())
          .filter(|duration| !duration.is_zero())
          .ok_or_else(|| self.taint(format!("browser CDP command {method} exceeded its deadline; command outcome is unknown")))?;
        if let Err(error) = configure_socket_read_timeout(&mut self.socket, remaining) {
          return Err(self.taint(format!("failed to update browser CDP deadline for {method}: {error}; command outcome is unknown")));
        }
        let message = match self.socket.read() {
          Ok(message) => message,
          Err(error) => {
            return Err(self.taint(format!("failed to read browser CDP response for {method}: {error}; command outcome is unknown")));
          }
        };
        match message {
          Message::Text(text) => {
            let response: Value = match serde_json::from_str(text.as_str()) {
              Ok(response) => response,
              Err(error) => return Err(self.taint(format!("browser CDP returned invalid JSON: {error}"))),
            };
            let response_id = response.get("id").and_then(Value::as_u64);
            if response_id.is_none() {
              continue;
            }
            if response_id != Some(id) {
              return Err(self.taint(format!("browser CDP response id {response_id:?} did not match outstanding request id {id}")));
            }
            let response_session = response.get("sessionId").and_then(Value::as_str);
            if response_session != session_id {
              return Err(self.taint(format!("browser CDP response session did not match outstanding {method} request")));
            }
            if let Some(error) = response.get("error") {
              let code = error.get("code").and_then(Value::as_i64).map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string());
              let message = error.get("message").and_then(Value::as_str).unwrap_or("unknown CDP error");
              return Err(backend(format!("browser CDP {method} failed ({code}): {message}")));
            }
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
          }
          Message::Ping(payload) => {
            let remaining = deadline
              .checked_duration_since(Instant::now())
              .filter(|duration| !duration.is_zero())
              .ok_or_else(|| self.taint(format!("browser CDP command {method} exceeded its deadline; command outcome is unknown")))?;
            if let Err(error) = configure_socket_write_timeout(&mut self.socket, remaining) {
              return Err(self.taint(format!("failed to update browser CDP ping deadline for {method}: {error}")));
            }
            if let Err(error) = self.socket.send(Message::Pong(payload)) {
              return Err(self.taint(format!("failed to answer browser CDP ping: {error}")));
            }
          }
          Message::Close(frame) => {
            return Err(self.taint(format!("browser CDP WebSocket closed while waiting for {method}: {frame:?}")));
          }
          Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
      }
    })();
    let deadline_expired = watchdog.finish() || deadline.checked_duration_since(Instant::now()).is_none();
    if deadline_expired {
      return Err(self.taint(format!("browser CDP command {method} exceeded its deadline; command outcome is unknown")));
    }
    result
  }

  fn taint(&mut self, message: impl Into<String>) -> DriverError {
    self.tainted = true;
    backend(message)
  }
}

#[derive(Debug, Deserialize)]
struct DomSnapshot {
  tag_name: String,
  text: String,
  text_truncated: bool,
  attributes: BTreeMap<String, String>,
  attributes_truncated: bool,
  viewport_bounds: Option<Rect>,
  visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameIdentity {
  frame_id: String,
  loader_id: String,
}

fn page_from_target(target: &Value) -> DriverResult<Page> {
  Ok(Page {
    reference: PageRef::new(required_str(target, "targetId", "Target.getTargets")?)?,
    url: required_str(target, "url", "Target.getTargets")?.to_string(),
    title: target.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
  })
}

fn frame_identity(result: &Value) -> DriverResult<FrameIdentity> {
  let frame = result
    .get("frameTree")
    .and_then(|tree| tree.get("frame"))
    .ok_or_else(|| backend("Page.getFrameTree response did not include frameTree.frame"))?;
  Ok(FrameIdentity {
    frame_id: required_str(frame, "id", "Page.getFrameTree")?.to_string(),
    loader_id: required_str(frame, "loaderId", "Page.getFrameTree")?.to_string(),
  })
}

fn dom_snapshot_bytes(snapshot: &DomSnapshot) -> usize {
  snapshot.tag_name.len().saturating_add(snapshot.text.len()).saturating_add(
    snapshot.attributes.iter().fold(0usize, |total, (name, value)| total.saturating_add(name.len()).saturating_add(value.len())),
  )
}

fn connect_cdp_socket(options: &BrowserConnectOptions, config: WebSocketConfig) -> DriverResult<CdpSocket> {
  let url = url::Url::parse(&options.websocket_url).map_err(|error| backend(format!("failed to parse browser CDP endpoint: {error}")))?;
  let host = url.host_str().ok_or_else(|| backend("browser CDP endpoint did not include a host"))?;
  let port = url.port_or_known_default().ok_or_else(|| backend("browser CDP endpoint did not include a usable port"))?;
  let deadline = Instant::now().checked_add(options.connect_timeout).ok_or_else(|| backend("browser CDP connect deadline overflowed"))?;
  let addresses = resolve_endpoint(host.trim_start_matches('[').trim_end_matches(']').to_string(), port, options.connect_timeout)?;
  if url.scheme() == "ws" && addresses.iter().any(|address| !address.ip().is_loopback()) {
    return Err(backend("unencrypted browser CDP endpoint resolved outside the loopback interface"));
  }
  let mut last_error: Option<String> = None;

  for address in addresses {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
      break;
    };
    match TcpStream::connect_timeout(&address, remaining) {
      Ok(stream) => {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
          break;
        };
        stream
          .set_nodelay(true)
          .and_then(|_| stream.set_read_timeout(Some(remaining)))
          .and_then(|_| stream.set_write_timeout(Some(remaining)))
          .map_err(|error| backend(format!("failed to configure browser CDP connection: {error}")))?;
        let cancel_stream =
          stream.try_clone().map_err(|error| backend(format!("failed to prepare browser CDP handshake cancellation: {error}")))?;
        let endpoint = options.websocket_url.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
          let result =
            client_tls_with_config(endpoint, stream, Some(config), None).map(|(socket, _)| socket).map_err(|error| error.to_string());
          let _ = sender.send(result);
        });
        let Some(handshake_remaining) = deadline.checked_duration_since(Instant::now()).filter(|duration| !duration.is_zero()) else {
          let _ = cancel_stream.shutdown(Shutdown::Both);
          let _ = worker.join();
          return Err(backend("timed out completing browser CDP WebSocket handshake"));
        };
        match receiver.recv_timeout(handshake_remaining) {
          Ok(Ok(socket)) => {
            let _ = worker.join();
            return Ok(socket);
          }
          Ok(Err(error)) => {
            let _ = worker.join();
            last_error = Some(error);
          }
          Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = cancel_stream.shutdown(Shutdown::Both);
            let _ = worker.join();
            return Err(backend("timed out completing browser CDP WebSocket handshake"));
          }
          Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            return Err(backend("browser CDP WebSocket handshake worker stopped unexpectedly"));
          }
        }
      }
      Err(error) => last_error = Some(error.to_string()),
    }
  }

  if deadline.checked_duration_since(Instant::now()).is_none() {
    return Err(backend("timed out connecting to browser CDP endpoint"));
  }
  Err(backend(format!("failed to connect to browser CDP endpoint{}", last_error.map(|error| format!(": {error}")).unwrap_or_default())))
}

fn resolve_endpoint(host: String, port: u16, timeout: Duration) -> DriverResult<Vec<SocketAddr>> {
  let (sender, receiver) = mpsc::sync_channel(1);
  thread::spawn(move || {
    let result = (host.as_str(), port).to_socket_addrs().map(|addresses| addresses.collect::<Vec<_>>()).map_err(|error| error.to_string());
    let _ = sender.send(result);
  });
  match receiver.recv_timeout(timeout) {
    Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
    Ok(Ok(_)) => Err(backend("browser CDP endpoint did not resolve to any network address")),
    Ok(Err(error)) => Err(backend(format!("failed to resolve browser CDP endpoint: {error}"))),
    Err(mpsc::RecvTimeoutError::Timeout) => Err(backend("timed out resolving browser CDP endpoint")),
    Err(mpsc::RecvTimeoutError::Disconnected) => Err(backend("browser CDP endpoint resolver stopped unexpectedly")),
  }
}

fn configure_socket_timeout(socket: &mut CdpSocket, timeout: Duration) -> DriverResult<()> {
  configure_socket_read_timeout(socket, timeout)?;
  configure_socket_write_timeout(socket, timeout)
}

fn configure_socket_write_timeout(socket: &mut CdpSocket, timeout: Duration) -> DriverResult<()> {
  socket_stream(socket)?
    .set_write_timeout(Some(timeout))
    .map_err(|error| backend(format!("failed to configure browser CDP socket write timeout: {error}")))
}

fn configure_socket_read_timeout(socket: &mut CdpSocket, timeout: Duration) -> DriverResult<()> {
  socket_stream(socket)?
    .set_read_timeout(Some(timeout))
    .map_err(|error| backend(format!("failed to configure browser CDP socket read timeout: {error}")))
}

fn socket_stream(socket: &mut CdpSocket) -> DriverResult<&mut TcpStream> {
  let stream = match socket.get_mut() {
    MaybeTlsStream::Plain(stream) => stream,
    MaybeTlsStream::Rustls(stream) => &mut stream.sock,
    _ => return Err(backend("browser CDP socket transport does not expose timeout controls")),
  };
  Ok(stream)
}

fn required_str<'a>(value: &'a Value, field: &str, operation: &str) -> DriverResult<&'a str> {
  value.get(field).and_then(Value::as_str).ok_or_else(|| backend(format!("{operation} response did not include string field {field}")))
}

fn required_f64(value: &Value, field: &str, operation: &str) -> DriverResult<f64> {
  value.get(field).and_then(Value::as_f64).ok_or_else(|| backend(format!("{operation} response did not include numeric field {field}")))
}

fn validate_capture_dimensions(width: f64, height: f64) -> DriverResult<()> {
  if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
    return Err(backend("browser screenshot dimensions must be positive finite values"));
  }
  if width > f64::from(MAX_CAPTURE_WIDTH) || height > f64::from(MAX_CAPTURE_HEIGHT) {
    return Err(backend(format!("browser screenshot exceeded the {MAX_CAPTURE_WIDTH}x{MAX_CAPTURE_HEIGHT} dimension limit")));
  }
  let pixels = (width.ceil() as u64).saturating_mul(height.ceil() as u64);
  if pixels > MAX_CAPTURE_PIXELS {
    return Err(backend(format!("browser screenshot exceeded the {MAX_CAPTURE_PIXELS}-pixel decode limit")));
  }
  Ok(())
}

fn decode_capture_png(encoded: &str) -> DriverResult<RgbaImage> {
  let bytes = base64::engine::general_purpose::STANDARD
    .decode(encoded)
    .map_err(|error| backend(format!("browser screenshot was not valid base64: {error}")))?;
  let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
  let mut limits = Limits::default();
  limits.max_image_width = Some(MAX_CAPTURE_WIDTH);
  limits.max_image_height = Some(MAX_CAPTURE_HEIGHT);
  limits.max_alloc = Some(MAX_CAPTURE_ALLOCATION);
  reader.limits(limits);
  let decoded =
    reader.decode().map_err(|error| backend(format!("browser screenshot exceeded decode limits or was not a valid PNG: {error}")))?;
  let (width, height) = decoded.dimensions();
  validate_capture_dimensions(f64::from(width), f64::from(height))?;
  Ok(decoded.into_rgba8())
}

fn stale_element(element: &ElementRef, error: DriverError) -> DriverError {
  DriverError::StaleObservation {
    message: format!(
      "browser element backend node {} on page {} is no longer actionable",
      element.backend_node_id(),
      element.page().as_str()
    ),
    recovery: Some(format!("query the element again before retrying; backend detail: {error}")),
  }
}

fn runtime_value(result: &Value, operation: &str) -> DriverResult<Value> {
  if let Some(exception) = result.get("exceptionDetails") {
    let detail = exception
      .get("exception")
      .and_then(|value| value.get("description"))
      .and_then(Value::as_str)
      .or_else(|| exception.get("text").and_then(Value::as_str))
      .unwrap_or("unknown JavaScript exception");
    return Err(backend(format!("{operation} failed: {detail}")));
  }
  result
    .get("result")
    .and_then(|remote| remote.get("value"))
    .cloned()
    .ok_or_else(|| backend(format!("{operation} did not return a JSON-serializable value")))
}

fn backend(message: impl Into<String>) -> DriverError {
  DriverError::Backend {
    message: message.into(),
  }
}

#[cfg(test)]
#[path = "cdp_test.rs"]
mod tests;
