#![warn(missing_docs)]

//! Chromium browser automation through the Chrome DevTools Protocol.
//!
//! This crate is a platform driver: it owns Chromium process or CDP connection
//! lifecycle and exposes page, DOM, capture, and browser-protocol input
//! capabilities. Application-specific website behavior belongs in a separate
//! Rust operation crate.
//!
//! Start a driver-owned local browser with
//! [`auv_driver_common::Driver::open_local`], or attach to an existing browser
//! with [`BrowserDriver::connect`]. A [`BrowserDriverSession`] keeps the
//! underlying CDP connection—and, for a locally launched browser, the child
//! process and temporary profile—alive. Dropping the last session releases
//! those resources.
//!
//! The API is synchronous. Connection establishment is bounded by
//! [`BrowserConnectOptions::connect_timeout`], while each CDP command is
//! bounded by [`BrowserConnectOptions::command_timeout`] (or
//! [`BrowserLaunchOptions::command_timeout`] for a local browser). A public
//! operation may issue several commands; navigation and DOM polling additionally
//! enforce their own overall wait deadlines.

mod cdp;
mod descriptor;
mod driver;
mod model;
mod session;

pub use descriptor::{BROWSER_CDP_CAPABILITIES, BrowserDriverDescriptor, browser_driver_descriptor};
pub use driver::{BrowserDriver, BrowserDriverSession};
pub use model::{
  BrowserConnectOptions, BrowserLaunchOptions, BrowserViewport, CssSelector, DomElement, ElementRef, NavigationOptions, NavigationWait,
  Page, PageCaptureOptions, PageRef,
};
pub use session::{DomApi, PageApi};
