use auv_driver_common::{DriverDescriptor, PlatformKind};

/// Capability identifiers implemented by the Chromium/CDP browser driver.
///
/// These identifiers describe the stable capability surface advertised by
/// [`browser_driver_descriptor`]. Website-specific operations are deliberately
/// excluded.
pub const BROWSER_CDP_CAPABILITIES: &[&str] = &[
  "browser.list-pages",
  "browser.open-page",
  "browser.close-page",
  "browser.navigate-page",
  "browser.reload-page",
  "browser.capture-page",
  "browser.evaluate-json",
  "browser.query-css",
  "browser.click-element",
  "browser.type-element-text",
  "browser.set-file-input-files",
  "browser.scroll-page",
];

/// Static identity and platform metadata for the Chromium/CDP driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserDriverDescriptor {
  /// Stable driver identifier (`"browser.cdp"`).
  pub id: &'static str,
  /// Platform family served by this driver.
  pub platform: PlatformKind,
  /// Human-readable summary of the implemented driver slice.
  pub description: &'static str,
}

impl BrowserDriverDescriptor {
  /// Converts this browser-specific descriptor to the shared driver contract.
  pub fn as_driver_descriptor(&self) -> DriverDescriptor {
    DriverDescriptor {
      id: self.id,
      platform: self.platform,
      description: self.description,
    }
  }
}

/// Returns the static descriptor for the Chromium/CDP browser driver.
pub fn browser_driver_descriptor() -> BrowserDriverDescriptor {
  BrowserDriverDescriptor {
    id: "browser.cdp",
    platform: PlatformKind::Browser,
    description: "Chromium browser driver: local headless launch or remote CDP connection, page lifecycle, DOM observation, screenshots, JSON evaluation, file inputs, and protocol input.",
  }
}

#[cfg(test)]
#[path = "descriptor_test.rs"]
mod tests;
