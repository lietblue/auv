use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use auv_driver_common::{Click, DriverError, DriverSession, InputDeliveryPath, PlatformKind, Scroll, WaitOptions};
use base64::Engine;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use serde_json::{Value, json};
use tungstenite::{Message, accept};

use crate::{BrowserConnectOptions, BrowserDriver, CssSelector, NavigationOptions, PageCaptureOptions, PageRef};

#[test]
fn public_browser_flow_uses_strict_dom_and_cdp_input_contracts() {
  let fake = FakeCdp::start();
  let session = BrowserDriver::connect(BrowserConnectOptions::new(&fake.websocket_url)).unwrap();

  assert_eq!(session.descriptor().platform, PlatformKind::Browser);
  let initial_pages = session.page().list().unwrap();
  assert_eq!(initial_pages.len(), 1);
  let download =
    session.page().navigate(&initial_pages[0].reference, "https://example.test/download", NavigationOptions::default()).unwrap_err();
  assert!(matches!(download, DriverError::Unsupported { .. }));

  let page = session.page().open("https://example.test/watch/42", NavigationOptions::default()).unwrap();
  assert_eq!(page.url, "https://example.test/watch/42");
  let page = session.page().reload(&page.reference, NavigationOptions::default()).unwrap();
  assert_eq!(page.url, "https://example.test/watch/42");

  let element = session.dom().resolve(&page.reference, &CssSelector::new("#record").unwrap()).unwrap();
  assert_eq!(element.tag_name, "button");
  assert_eq!(element.text, "Record");
  assert!(!element.text_truncated);
  assert!(!element.attributes_truncated);
  assert!(element.visible);

  let click = session.dom().click(&element, Click::Single).unwrap();
  assert_eq!(click.selected_path, InputDeliveryPath::CdpInput);
  let double_click = session
    .dom()
    .click(
      &element,
      Click::Double {
        interval: Duration::ZERO,
      },
    )
    .unwrap();
  assert_eq!(double_click.selected_path, InputDeliveryPath::CdpInput);
  let typed = session.dom().type_text(&element, "hello browser").unwrap();
  assert_eq!(typed.selected_path, InputDeliveryPath::CdpInput);
  let oversized_input = "x".repeat(1_024 * 1_024);
  let oversized_error = session.dom().type_text(&element, &oversized_input).unwrap_err();
  assert!(oversized_error.to_string().contains("request limit"));
  assert!(!session.page().list().unwrap().is_empty());
  let scrolled = session.page().scroll(&page.reference, Scroll::new(0.0, 480.0)).unwrap();
  assert_eq!(scrolled.selected_path, InputDeliveryPath::CdpInput);

  let evaluated = session.page().evaluate(&page.reference, "({ answer: 42 })").unwrap();
  assert_eq!(evaluated, json!({ "answer": 42 }));
  let unserializable = session.page().evaluate(&page.reference, "undefined").unwrap_err();
  assert!(unserializable.to_string().contains("not JSON-serializable"));
  let exception = session.page().evaluate(&page.reference, "throw new Error('boom')").unwrap_err();
  assert!(exception.to_string().contains("boom"));
  let capture = session.page().capture(&page.reference, PageCaptureOptions { full_page: true }).unwrap();
  assert_eq!(capture.image.dimensions(), (2, 1));
  assert_eq!(capture.bounds.size.width, 1.0);
  assert_eq!(capture.bounds.size.height, 0.5);
  assert_eq!(capture.scale_factor, 2.0);
  assert_eq!(capture.backend, "browser.cdp");

  let ambiguous = session.dom().resolve(&page.reference, &CssSelector::new(".many").unwrap()).unwrap_err();
  assert!(ambiguous.to_string().contains("matched 2 elements"));
  let too_broad = session.dom().query_all(&page.reference, &CssSelector::new(".too-many").unwrap()).unwrap_err();
  assert!(too_broad.to_string().contains("observation limit"));
  let too_large = session.dom().query_all(&page.reference, &CssSelector::new(".too-large").unwrap()).unwrap_err();
  assert!(too_large.to_string().contains("byte browser observation limit"));
  let second = session.dom().resolve(&page.reference, &CssSelector::new(".many").unwrap().at(1)).unwrap();
  assert_eq!(second.reference.backend_node_id(), 103);
  let blocked = session.dom().resolve(&page.reference, &CssSelector::new(".blocked").unwrap()).unwrap();
  let blocked_error = session.dom().click(&blocked, Click::Single).unwrap_err();
  assert!(blocked_error.to_string().contains("obscured"));

  let reloaded = session.page().reload(&page.reference, NavigationOptions::default()).unwrap();
  let stale_error = session.dom().click(&element, Click::Single).unwrap_err();
  assert!(stale_error.to_string().contains("earlier document"));

  session.page().close(&reloaded.reference).unwrap();
  drop(session);
  let methods = fake.finish();
  for expected in [
    "Target.getTargets",
    "Target.createTarget",
    "Target.attachToTarget",
    "Page.navigate",
    "Runtime.evaluate",
    "Runtime.callFunctionOn",
    "DOM.querySelectorAll",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.captureScreenshot",
    "Target.closeTarget",
  ] {
    assert!(methods.iter().any(|method| method == expected), "missing fake CDP call {expected}");
  }
}

#[test]
fn command_deadline_taints_connection_even_when_events_keep_arriving() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || serve_event_flood(listener));
  let mut options = BrowserConnectOptions::new(format!("ws://{address}/devtools/browser/flood"));
  options.command_timeout = Duration::from_millis(100);
  let session = BrowserDriver::connect(options).unwrap();

  let started = Instant::now();
  let first = session.page().list().unwrap_err();
  assert!(first.to_string().contains("outcome is unknown"));
  assert!(started.elapsed() < Duration::from_secs(2));

  let second = session.page().list().unwrap_err();
  assert!(second.to_string().contains("no longer reusable"));
  drop(session);
  assert_eq!(server.join().unwrap(), 1);
}

#[test]
fn command_deadline_interrupts_a_partial_frame_slowloris() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || serve_partial_frame_slowloris(listener));
  let mut options = BrowserConnectOptions::new(format!("ws://{address}/devtools/browser/partial-frame"));
  options.command_timeout = Duration::from_millis(100);
  let session = BrowserDriver::connect(options).unwrap();

  let started = Instant::now();
  let error = session.page().list().unwrap_err();
  assert!(error.to_string().contains("outcome is unknown"));
  assert!(started.elapsed() < Duration::from_secs(2));

  drop(session);
  assert_eq!(server.join().unwrap(), 1);
}

#[test]
fn dom_wait_deadline_bounds_in_flight_cdp_commands() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || serve_wait_snapshot_flood(listener));
  let mut options = BrowserConnectOptions::new(format!("ws://{address}/devtools/browser/wait-flood"));
  options.command_timeout = Duration::from_secs(1);
  let session = BrowserDriver::connect(options).unwrap();
  let page = PageRef::new("page-1").unwrap();
  let selector = CssSelector::new(".missing").unwrap();

  let started = Instant::now();
  let error = session
    .dom()
    .wait(
      &page,
      &selector,
      WaitOptions {
        timeout: Duration::from_millis(100),
        poll_interval: Duration::from_millis(5),
      },
    )
    .unwrap_err();
  assert!(error.to_string().contains("outcome is unknown"));
  assert!(started.elapsed() < Duration::from_secs(2));

  drop(session);
  let methods = server.join().unwrap();
  assert!(methods.iter().any(|method| method == "Runtime.callFunctionOn"));
}

#[test]
fn failed_page_session_initialization_detaches_the_session() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || serve_page_enable_failure(listener));
  let session = BrowserDriver::connect(BrowserConnectOptions::new(format!("ws://{address}/devtools/browser/enable-failure"))).unwrap();
  let page = PageRef::new("page-1").unwrap();

  let error = session.dom().query_all(&page, &CssSelector::new("button").unwrap()).unwrap_err();
  assert!(error.to_string().contains("Page.enable failed"));

  drop(session);
  let methods = server.join().unwrap();
  assert_eq!(methods.iter().filter(|method| method.as_str() == "Target.attachToTarget").count(), 1);
  assert_eq!(methods.iter().filter(|method| method.as_str() == "Target.detachFromTarget").count(), 1);
}

#[test]
fn websocket_handshake_timeout_cancels_and_joins_the_worker() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || serve_stalled_handshake(listener));
  let mut options = BrowserConnectOptions::new(format!("ws://{address}/devtools/browser/stalled"));
  options.connect_timeout = Duration::from_millis(100);

  let started = Instant::now();
  let error = BrowserDriver::connect(options).unwrap_err();
  assert!(matches!(error, DriverError::Backend { .. }));
  assert!(error.to_string().contains("browser CDP"));
  assert!(started.elapsed() < Duration::from_secs(2));
  assert_eq!(server.join().unwrap(), 1);
}

struct FakeCdp {
  websocket_url: String,
  handle: JoinHandle<Vec<String>>,
}

impl FakeCdp {
  fn start() -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || serve(listener));
    Self {
      websocket_url: format!("ws://{address}/devtools/browser/fake"),
      handle,
    }
  }

  fn finish(self) -> Vec<String> {
    self.handle.join().unwrap()
  }
}

fn serve(listener: TcpListener) -> Vec<String> {
  let (stream, _) = listener.accept().unwrap();
  let mut socket = accept(stream).unwrap();
  let mut pages = BTreeMap::from([("page-1".to_string(), "about:blank".to_string())]);
  let mut page_loaders = BTreeMap::from([("page-1".to_string(), "loader-page-1".to_string())]);
  let mut pending_loaders = BTreeMap::<String, String>::new();
  let mut sessions = BTreeMap::<String, String>::new();
  let mut methods = Vec::new();
  let screenshot = tiny_png();
  let mut sent_event = false;
  let mut mouse_moved = false;
  let mut box_observed_after_mouse_move = false;

  loop {
    let message = match socket.read() {
      Ok(message) => message,
      Err(_) => break,
    };
    let Message::Text(text) = message else {
      if matches!(message, Message::Close(_)) {
        break;
      }
      continue;
    };
    let request: Value = serde_json::from_str(text.as_str()).unwrap();
    let id = request["id"].as_u64().unwrap();
    let method = request["method"].as_str().unwrap().to_string();
    methods.push(method.clone());

    if !sent_event {
      socket
        .send(Message::Text(
          json!({
            "method": "Target.targetInfoChanged",
            "params": { "targetInfo": target("page-1", "about:blank") }
          })
          .to_string()
          .into(),
        ))
        .unwrap();
      sent_event = true;
    }

    let result = match method.as_str() {
      "Target.getTargets" => {
        let target_infos = pages.iter().map(|(id, url)| target(id, url)).collect::<Vec<_>>();
        json!({ "targetInfos": target_infos })
      }
      "Target.createTarget" => {
        pages.insert("page-2".to_string(), "about:blank".to_string());
        page_loaders.insert("page-2".to_string(), "loader-page-2-initial".to_string());
        json!({ "targetId": "page-2" })
      }
      "Target.attachToTarget" => {
        let page = request["params"]["targetId"].as_str().unwrap();
        let session = format!("session-{page}");
        sessions.insert(session.clone(), page.to_string());
        json!({ "sessionId": session })
      }
      "Target.closeTarget" => {
        let page = request["params"]["targetId"].as_str().unwrap();
        pages.remove(page);
        page_loaders.remove(page);
        json!({ "success": true })
      }
      "Page.enable"
      | "Runtime.enable"
      | "DOM.enable"
      | "DOM.focus"
      | "DOM.scrollIntoViewIfNeeded"
      | "Runtime.releaseObject"
      | "Input.insertText" => json!({}),
      "Input.dispatchMouseEvent" => {
        match request["params"]["type"].as_str() {
          Some("mouseMoved") => {
            mouse_moved = true;
            box_observed_after_mouse_move = false;
          }
          Some("mousePressed") => {
            mouse_moved = false;
            box_observed_after_mouse_move = false;
          }
          _ => {}
        }
        json!({})
      }
      "Page.navigate" => {
        let session = request["sessionId"].as_str().unwrap();
        let page = sessions.get(session).unwrap();
        let url = request["params"]["url"].as_str().unwrap();
        if url.ends_with("/download") {
          json!({ "frameId": format!("frame-{page}"), "isDownload": true })
        } else {
          pages.insert(page.clone(), url.to_string());
          let loader_id = format!("loader-{page}-navigated-{}", methods.len());
          pending_loaders.insert(page.clone(), loader_id.clone());
          json!({ "frameId": format!("frame-{page}"), "loaderId": loader_id })
        }
      }
      "Page.reload" => {
        let session = request["sessionId"].as_str().unwrap();
        let page = sessions.get(session).unwrap();
        pending_loaders.insert(page.clone(), format!("loader-{page}-reloaded-{}", methods.len()));
        json!({})
      }
      "Page.getFrameTree" => {
        let session = request["sessionId"].as_str().unwrap();
        let page = sessions.get(session).unwrap();
        let result = json!({
          "frameTree": {
            "frame": {
              "id": format!("frame-{page}"),
              "loaderId": page_loaders.get(page).unwrap(),
              "url": pages.get(page).unwrap(),
              "securityOrigin": "https://example.test",
              "mimeType": "text/html",
            }
          }
        });
        if let Some(loader_id) = pending_loaders.remove(page) {
          page_loaders.insert(page.clone(), loader_id);
        }
        result
      }
      "Page.createIsolatedWorld" => json!({ "executionContextId": 7 }),
      "Runtime.evaluate" => {
        let expression = request["params"]["expression"].as_str().unwrap();
        if expression == "document.readyState" {
          assert_eq!(request["params"]["contextId"].as_u64(), Some(7));
          json!({ "result": { "type": "string", "value": "complete" } })
        } else if expression == "undefined" {
          assert!(request["params"].get("contextId").is_none());
          json!({ "result": { "type": "undefined" } })
        } else if expression.starts_with("throw new Error") {
          assert!(request["params"].get("contextId").is_none());
          json!({
            "result": { "type": "object", "subtype": "error" },
            "exceptionDetails": {
              "text": "Uncaught",
              "exception": { "description": "Error: boom" }
            }
          })
        } else if expression.starts_with("({ x:") {
          json!({ "result": { "type": "object", "value": { "x": 640.0, "y": 360.0 } } })
        } else {
          assert!(request["params"].get("contextId").is_none());
          json!({ "result": { "type": "object", "value": { "answer": 42 } } })
        }
      }
      "Runtime.callFunctionOn" => {
        let function = request["params"]["functionDeclaration"].as_str().unwrap();
        if function.contains("elementFromPoint") {
          assert!(box_observed_after_mouse_move, "click hit testing must follow hover and a fresh box observation");
          let receives_pointer = request["params"]["objectId"].as_str() != Some("object-104");
          json!({ "result": { "type": "boolean", "value": receives_pointer } })
        } else if function.contains("activeElement") {
          json!({ "result": { "type": "boolean", "value": true } })
        } else if request["params"]["objectId"].as_str() == Some("object-105") {
          let mut snapshot = dom_snapshot();
          snapshot["text"] = Value::String("x".repeat(2 * 1_024 * 1_024 + 1));
          json!({ "result": { "type": "object", "value": snapshot } })
        } else {
          json!({ "result": { "type": "object", "value": dom_snapshot() } })
        }
      }
      "DOM.getDocument" => json!({ "root": { "nodeId": 1 } }),
      "DOM.querySelectorAll" => {
        let node_ids = match request["params"]["selector"].as_str().unwrap() {
          ".many" => vec![2, 3],
          ".too-many" => (2..=130).collect(),
          ".too-large" => vec![5],
          ".blocked" => vec![4],
          ".missing" => Vec::new(),
          _ => vec![2],
        };
        json!({ "nodeIds": node_ids })
      }
      "DOM.describeNode" => {
        let node_id = request["params"]["nodeId"].as_u64().unwrap();
        json!({ "node": { "nodeId": node_id, "backendNodeId": node_id + 100 } })
      }
      "DOM.resolveNode" => {
        let backend_node_id = request["params"]["backendNodeId"].as_u64().unwrap();
        assert_eq!(request["params"]["executionContextId"].as_u64(), Some(7));
        json!({ "object": { "type": "object", "objectId": format!("object-{backend_node_id}") } })
      }
      "DOM.getBoxModel" => {
        assert!(request["params"].get("executionContextId").is_none());
        if mouse_moved {
          box_observed_after_mouse_move = true;
        }
        json!({ "model": { "content": [10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0] } })
      }
      "Page.getLayoutMetrics" => json!({
        "cssContentSize": { "width": 1.0, "height": 0.5 },
        "cssVisualViewport": { "clientWidth": 1.0, "clientHeight": 0.5 },
      }),
      "Page.captureScreenshot" => json!({ "data": screenshot }),
      unexpected => panic!("unexpected fake CDP method {unexpected}"),
    };
    let mut response = json!({ "id": id, "result": result });
    if let Some(session_id) = request.get("sessionId") {
      response["sessionId"] = session_id.clone();
    }
    socket.send(Message::Text(response.to_string().into())).unwrap();
  }

  methods
}

fn serve_event_flood(listener: TcpListener) -> usize {
  let (stream, _) = listener.accept().unwrap();
  let mut socket = accept(stream).unwrap();
  let request = socket.read().unwrap();
  assert!(matches!(request, Message::Text(_)));

  let started = Instant::now();
  while started.elapsed() < Duration::from_millis(500) {
    if socket.send(Message::Text(json!({ "method": "Runtime.consoleAPICalled", "params": {} }).to_string().into())).is_err() {
      break;
    }
    thread::sleep(Duration::from_millis(5));
  }
  1
}

fn serve_partial_frame_slowloris(listener: TcpListener) -> usize {
  let (stream, _) = listener.accept().unwrap();
  let mut socket = accept(stream).unwrap();
  let request = socket.read().unwrap();
  assert!(matches!(request, Message::Text(_)));
  let mut stream = socket.into_inner();
  stream.write_all(&[0x81, 100]).unwrap();
  for _ in 0..100 {
    if stream.write_all(b"x").is_err() {
      break;
    }
    let _ = stream.flush();
    thread::sleep(Duration::from_millis(5));
  }
  1
}

fn serve_wait_snapshot_flood(listener: TcpListener) -> Vec<String> {
  let (stream, _) = listener.accept().unwrap();
  let mut socket = accept(stream).unwrap();
  let mut methods = Vec::new();

  loop {
    let message = match socket.read() {
      Ok(message) => message,
      Err(_) => break,
    };
    let Message::Text(text) = message else {
      continue;
    };
    let request: Value = serde_json::from_str(text.as_str()).unwrap();
    let id = request["id"].as_u64().unwrap();
    let method = request["method"].as_str().unwrap().to_string();
    methods.push(method.clone());

    if method == "Runtime.callFunctionOn" {
      let started = Instant::now();
      while started.elapsed() < Duration::from_millis(500) {
        if socket.send(Message::Text(json!({ "method": "Runtime.consoleAPICalled", "params": {} }).to_string().into())).is_err() {
          break;
        }
        thread::sleep(Duration::from_millis(5));
      }
      break;
    }

    let result = match method.as_str() {
      "Target.getTargets" => json!({ "targetInfos": [target("page-1", "about:blank")] }),
      "Target.attachToTarget" => json!({ "sessionId": "session-page-1" }),
      "Page.enable" | "Runtime.enable" | "DOM.enable" => json!({}),
      "Page.getFrameTree" => json!({
        "frameTree": {
          "frame": {
            "id": "frame-page-1",
            "loaderId": "loader-page-1",
          }
        }
      }),
      "Page.createIsolatedWorld" => json!({ "executionContextId": 7 }),
      "DOM.getDocument" => json!({ "root": { "nodeId": 1 } }),
      "DOM.querySelectorAll" => json!({ "nodeIds": [2] }),
      "DOM.describeNode" => json!({ "node": { "backendNodeId": 102 } }),
      "DOM.resolveNode" => json!({ "object": { "objectId": "object-102" } }),
      unexpected => panic!("unexpected fake CDP method {unexpected}"),
    };
    let mut response = json!({ "id": id, "result": result });
    if let Some(session_id) = request.get("sessionId") {
      response["sessionId"] = session_id.clone();
    }
    socket.send(Message::Text(response.to_string().into())).unwrap();
  }
  methods
}

fn serve_page_enable_failure(listener: TcpListener) -> Vec<String> {
  let (stream, _) = listener.accept().unwrap();
  let mut socket = accept(stream).unwrap();
  let mut methods = Vec::new();

  while let Ok(message) = socket.read() {
    let Message::Text(text) = message else {
      if matches!(message, Message::Close(_)) {
        break;
      }
      continue;
    };
    let request: Value = serde_json::from_str(text.as_str()).unwrap();
    let id = request["id"].as_u64().unwrap();
    let method = request["method"].as_str().unwrap().to_string();
    methods.push(method.clone());

    let mut response = match method.as_str() {
      "Target.getTargets" => json!({
        "id": id,
        "result": { "targetInfos": [target("page-1", "about:blank")] },
      }),
      "Target.attachToTarget" => json!({
        "id": id,
        "result": { "sessionId": "session-page-1" },
      }),
      "Page.enable" => json!({
        "id": id,
        "error": { "code": -32000, "message": "injected enable failure" },
      }),
      "Target.detachFromTarget" => json!({ "id": id, "result": {} }),
      unexpected => panic!("unexpected fake CDP method {unexpected}"),
    };
    if let Some(session_id) = request.get("sessionId") {
      response["sessionId"] = session_id.clone();
    }
    if socket.send(Message::Text(response.to_string().into())).is_err() {
      break;
    }
  }
  methods
}

fn serve_stalled_handshake(listener: TcpListener) -> usize {
  let (mut stream, _) = listener.accept().unwrap();
  let mut buffer = [0u8; 4096];
  while let Ok(read) = stream.read(&mut buffer) {
    if read == 0 {
      break;
    }
  }
  1
}

fn target(id: &str, url: &str) -> Value {
  json!({
    "targetId": id,
    "type": "page",
    "title": "Fake page",
    "url": url,
    "attached": false,
  })
}

fn dom_snapshot() -> Value {
  json!({
    "tag_name": "button",
    "text": "Record",
    "text_truncated": false,
    "attributes": { "id": "record" },
    "attributes_truncated": false,
    "viewport_bounds": {
      "origin": { "x": 10.0, "y": 20.0 },
      "size": { "width": 100.0, "height": 40.0 }
    },
    "visible": true,
  })
}

fn tiny_png() -> String {
  let image = RgbaImage::from_pixel(2, 1, Rgba([12, 34, 56, 255]));
  let mut bytes = Cursor::new(Vec::new());
  DynamicImage::ImageRgba8(image).write_to(&mut bytes, ImageFormat::Png).unwrap();
  base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
}
