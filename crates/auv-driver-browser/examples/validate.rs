use std::error::Error;

use auv_driver_browser::{BrowserDriver, CssSelector, NavigationOptions, PageCaptureOptions};
use auv_driver_common::{Click, Driver};

fn main() -> Result<(), Box<dyn Error>> {
  let session = BrowserDriver::new().open_local()?;
  let page = session.page().open(
    "data:text/html,%3Cbutton%20id%3D%22auv%22%20onclick%3D%22this.textContent%3D%27clicked%27%22%3Eready%3C%2Fbutton%3E",
    NavigationOptions::default(),
  )?;
  let button = session.dom().resolve(&page.reference, &CssSelector::new("#auv")?)?;
  session.dom().click(&button, Click::Single)?;
  let text = session.page().evaluate(&page.reference, "document.querySelector('#auv').textContent")?;
  let capture = session.page().capture(&page.reference, PageCaptureOptions::default())?;

  println!("browser validation completed: text={text}, capture={}x{}", capture.image.width(), capture.image.height());
  Ok(())
}
