use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use auv_driver_common::{Capture, Click, DriverError, DriverResult, InputActionResult, Scroll, WaitOptions};
use serde_json::Value;

use crate::driver::BrowserDriverSession;
use crate::model::{CssSelector, DomElement, NavigationOptions, Page, PageCaptureOptions, PageRef, validate_url, validate_wait};

/// Borrowed page-lifecycle and page-level operation facade.
///
/// This lightweight value does not own the session. Its methods synchronously
/// serialize work through the session's shared CDP connection.
#[derive(Clone, Copy, Debug)]
pub struct PageApi<'a> {
  session: &'a BrowserDriverSession,
}

/// Borrowed DOM observation and element-action facade.
///
/// This lightweight value does not own the session. Element observations
/// produced here are session-bound and become stale after navigation.
#[derive(Clone, Copy, Debug)]
pub struct DomApi<'a> {
  session: &'a BrowserDriverSession,
}

impl BrowserDriverSession {
  /// Borrows the session's page operation facade.
  pub fn page(&self) -> PageApi<'_> {
    PageApi { session: self }
  }

  /// Borrows the session's DOM operation facade.
  pub fn dom(&self) -> DomApi<'_> {
    DomApi { session: self }
  }
}

impl PageApi<'_> {
  /// Lists the currently open Chromium targets whose type is `page`.
  pub fn list(&self) -> DriverResult<Vec<Page>> {
    self.session.backend.list_pages()
  }

  /// Resolves current URL and title metadata for `page`.
  ///
  /// Returns [`DriverError::NotFound`] after a target has closed.
  pub fn resolve(&self, page: &PageRef) -> DriverResult<Page> {
    self.session.backend.resolve_page(page)
  }

  /// Opens a page target, navigates it to `url`, and waits for readiness.
  ///
  /// `url` must be an absolute parseable URL. The total navigation wait is
  /// bounded by [`NavigationOptions::wait`]; downloads are not treated as page
  /// navigations.
  pub fn open(&self, url: &str, options: NavigationOptions) -> DriverResult<Page> {
    validate_url(url)?;
    validate_wait(options.wait)?;
    self.session.backend.open_page(url, options)
  }

  /// Closes the referenced page target.
  pub fn close(&self, page: &PageRef) -> DriverResult<()> {
    self.session.backend.close_page(page)
  }

  /// Navigates an existing page and waits for the new committed document.
  ///
  /// The method verifies both a loader-generation change and the readiness
  /// requested by `options`, so an already-ready previous document cannot
  /// satisfy the wait. Downloads are rejected.
  pub fn navigate(&self, page: &PageRef, url: &str, options: NavigationOptions) -> DriverResult<Page> {
    validate_url(url)?;
    validate_wait(options.wait)?;
    self.session.backend.navigate(page, url, options)
  }

  /// Reloads a page and waits for a new committed document to become ready.
  pub fn reload(&self, page: &PageRef, options: NavigationOptions) -> DriverResult<Page> {
    validate_wait(options.wait)?;
    self.session.backend.reload(page, options)
  }

  /// Captures the viewport or full page as decoded RGBA pixels.
  ///
  /// Chromium supplies a PNG, which the driver validates and decodes into the
  /// returned [`Capture`]. Both logical and decoded dimensions are limited to
  /// 32,768 pixels per axis and 20,971,520 pixels total; image decoding is
  /// limited to 256 MiB of allocation.
  ///
  /// [`Capture::bounds`] use logical CSS pixels in the returned image's local
  /// coordinate space. A viewport capture starts at `(0, 0)` even when the
  /// document is scrolled; a full-page capture starts at the document origin.
  /// [`Capture::scale_factor`] is derived from decoded pixel dimensions divided
  /// by those logical bounds, so callers must use it rather than assuming
  /// `1.0`, especially for attached remote browsers.
  pub fn capture(&self, page: &PageRef, options: PageCaptureOptions) -> DriverResult<Capture> {
    self.session.backend.capture(page, options)
  }

  /// Evaluates JavaScript in the page's main world and returns a JSON value.
  ///
  /// This is a **high-trust** escape hatch: the expression runs with the page's
  /// origin and can read or mutate application state. Do not interpolate
  /// untrusted input into `expression`. Promises are awaited, but DOM nodes,
  /// `undefined`, non-finite numbers, and other values that CDP cannot return by
  /// value are rejected instead of being coerced to JSON `null`.
  ///
  /// JavaScript execution and each protocol command are bounded by the
  /// session's command timeout.
  pub fn evaluate(&self, page: &PageRef, expression: &str) -> DriverResult<Value> {
    if expression.trim().is_empty() {
      return Err(DriverError::InvalidInput {
        message: "browser JavaScript expression must not be empty".to_string(),
      });
    }
    self.session.backend.evaluate_json(page, expression)
  }

  /// Dispatches a CDP mouse-wheel event at the center of the visual viewport.
  ///
  /// Both deltas must be finite. The returned action result records
  /// [`auv_driver_common::InputDeliveryPath::CdpInput`] as its delivery path.
  pub fn scroll(&self, page: &PageRef, scroll: Scroll) -> DriverResult<InputActionResult> {
    if !scroll.delta_x.is_finite() || !scroll.delta_y.is_finite() {
      return Err(DriverError::InvalidInput {
        message: "browser scroll deltas must be finite".to_string(),
      });
    }
    self.session.backend.scroll(page, scroll)
  }
}

impl DomApi<'_> {
  /// Observes elements matching `selector` in the current top-level document.
  ///
  /// Without [`CssSelector::at`], more than 128 matches are rejected and all
  /// matching elements are returned. With an index, the result contains zero or
  /// one element. Each [`DomElement`] is bounded and exposes
  /// [`DomElement::text_truncated`] and
  /// [`DomElement::attributes_truncated`]; the whole result is also limited to
  /// a 2 MiB observation budget.
  ///
  /// The driver verifies that the document did not change during the query.
  pub fn query_all(&self, page: &PageRef, selector: &CssSelector) -> DriverResult<Vec<DomElement>> {
    self.session.backend.query_all(page, selector)
  }

  /// Resolves exactly one element matching `selector`.
  ///
  /// An unindexed selector must match exactly once. Zero matches return
  /// [`DriverError::NotFound`], while multiple matches return
  /// [`DriverError::InvalidInput`] and require an explicit zero-based
  /// [`CssSelector::at`] index.
  pub fn resolve(&self, page: &PageRef, selector: &CssSelector) -> DriverResult<DomElement> {
    select_element(self.query_all(page, selector)?, selector)
  }

  /// Polls until `selector` resolves to one element or the wait expires.
  ///
  /// [`WaitOptions::timeout`] is an absolute deadline for the entire wait,
  /// including every nested CDP call; events or polling do not reset it.
  /// [`WaitOptions::poll_interval`] controls delay between not-found results.
  /// Ambiguous selectors and protocol failures are returned immediately rather
  /// than retried.
  pub fn wait(&self, page: &PageRef, selector: &CssSelector, options: WaitOptions) -> DriverResult<DomElement> {
    validate_wait(options)?;
    let deadline = Instant::now().checked_add(options.timeout).ok_or_else(|| DriverError::InvalidInput {
      message: "browser DOM wait deadline overflowed".to_string(),
    })?;
    loop {
      let remaining =
        deadline.checked_duration_since(Instant::now()).filter(|duration| !duration.is_zero()).ok_or_else(|| DriverError::NotFound {
          target: format!("CSS selector {:?} before wait timeout", selector.as_str()),
        })?;
      let result =
        self.session.backend.query_all_with_timeout(page, selector, remaining).and_then(|elements| select_element(elements, selector));
      match result {
        Ok(element) => return Ok(element),
        Err(DriverError::NotFound { .. }) => {
          let remaining =
            deadline.checked_duration_since(Instant::now()).filter(|duration| !duration.is_zero()).ok_or_else(|| DriverError::NotFound {
              target: format!("CSS selector {:?} before wait timeout", selector.as_str()),
            })?;
          thread::sleep(options.poll_interval.min(remaining));
        }
        Err(error) => return Err(error),
      }
    }
  }

  /// Scrolls an observed element into view and dispatches a single or double
  /// left click at its current center.
  ///
  /// The driver revalidates document identity, node identity, geometry, and hit
  /// testing before dispatch. A moved, detached, navigated, or obscured element
  /// produces a stale-observation error and should be queried again.
  pub fn click(&self, element: &DomElement, click: Click) -> DriverResult<InputActionResult> {
    self.session.backend.click(element, click)
  }

  /// Focuses an observed element and inserts `text` through CDP.
  ///
  /// Existing content is not cleared and no submit key is sent. The element and
  /// its document are revalidated before input; stale observations should be
  /// queried again.
  pub fn type_text(&self, element: &DomElement, text: &str) -> DriverResult<InputActionResult> {
    self.session.backend.type_text(element, text)
  }

  /// Replaces the selected files of an observed `<input type="file">`.
  ///
  /// Paths are canonicalized and must point to existing regular files. An
  /// empty slice clears the current selection. At most 128 files can be
  /// selected in one call. Chromium dispatches the corresponding file-input
  /// events; this method does not open a native file chooser.
  pub fn set_file_input_files(&self, element: &DomElement, files: &[PathBuf]) -> DriverResult<InputActionResult> {
    if element.tag_name != "input" || !element.attributes.get("type").is_some_and(|value| value.eq_ignore_ascii_case("file")) {
      return Err(DriverError::InvalidInput {
        message: "browser file selection requires an observed <input type=\"file\"> element".to_string(),
      });
    }
    if files.len() > 128 {
      return Err(DriverError::InvalidInput {
        message: "browser file selection supports at most 128 files per input".to_string(),
      });
    }
    let files = files.iter().map(|path| canonical_file(path)).collect::<DriverResult<Vec<_>>>()?;
    self.session.backend.set_file_input_files(element, &files)
  }
}

fn canonical_file(path: &Path) -> DriverResult<String> {
  let canonical = path.canonicalize().map_err(|error| DriverError::InvalidInput {
    message: format!("browser upload file {} could not be resolved: {error}", path.display()),
  })?;
  if !canonical.is_file() {
    return Err(DriverError::InvalidInput {
      message: format!("browser upload path is not a regular file: {}", canonical.display()),
    });
  }
  canonical.to_str().map(str::to_string).ok_or_else(|| DriverError::InvalidInput {
    message: format!("browser upload file path is not valid UTF-8: {}", canonical.display()),
  })
}

fn select_element(elements: Vec<DomElement>, selector: &CssSelector) -> DriverResult<DomElement> {
  if let Some(index) = selector.index() {
    return elements.into_iter().next().ok_or_else(|| DriverError::NotFound {
      target: format!("CSS selector {:?} at index {index}", selector.as_str()),
    });
  }
  match elements.len() {
    0 => Err(DriverError::NotFound {
      target: format!("CSS selector {:?}", selector.as_str()),
    }),
    1 => Ok(elements.into_iter().next().expect("one element was observed")),
    count => Err(DriverError::InvalidInput {
      message: format!("CSS selector {:?} matched {count} elements; use CssSelector::at(index) to choose one", selector.as_str()),
    }),
  }
}

#[cfg(test)]
#[path = "session_test.rs"]
mod tests;
