//! Reading a QR code with the camera: the browser's video, a frame of which is
//! sent to the server to decode every now and then.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaStream,
    MediaStreamConstraints, MediaStreamTrack,
};

/// The longest side of a frame sent to decode, in pixels.
const FRAME_MAX: u32 = 960;

/// The camera popup.
pub struct Camera {
    /// The video, once the browser grants it.
    pub stream: Option<MediaStream>,
    /// Keeps the frame loop going.
    pub running: Rc<Cell<bool>>,
    /// A frame is being decoded.
    pub decoding: bool,
    /// Why there is no video, if so.
    pub error: Option<String>,
}

impl Camera {
    pub fn new() -> Self {
        Self {
            stream: None,
            running: Rc::new(Cell::new(true)),
            decoding: false,
            error: None,
        }
    }

    /// Stop the video and the frame loop.
    pub fn stop(&self) {
        self.running.set(false);
        if let Some(stream) = &self.stream {
            stop(stream);
        }
    }
}

/// Ask the browser for the camera: the rear one, on phones.
pub async fn open() -> Result<MediaStream, String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let devices = window.navigator().media_devices().map_err(|_| {
        "this page cannot use the camera: browsers allow it on https or localhost only".to_string()
    })?;

    let video = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&video, &"facingMode".into(), &"environment".into());
    let constraints = MediaStreamConstraints::new();
    constraints.set_video(&video);

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(describe)?;
    let stream = JsFuture::from(promise).await.map_err(describe)?;

    Ok(stream.unchecked_into())
}

/// Stop every track of `stream`, turning the camera off.
pub fn stop(stream: &MediaStream) {
    for track in stream.get_tracks().iter() {
        track.unchecked_into::<MediaStreamTrack>().stop();
    }
}

/// The current frame of `video` as a JPEG, base64-encoded; `None` before it
/// plays.
pub fn frame(video: &HtmlVideoElement) -> Option<String> {
    let (width, height) = (video.video_width(), video.video_height());
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (f64::from(FRAME_MAX) / f64::from(width.max(height))).min(1.0);
    let (width, height) = (
        (f64::from(width) * scale) as u32,
        (f64::from(height) * scale) as u32,
    );

    let canvas: HtmlCanvasElement = web_sys::window()?
        .document()?
        .create_element("canvas")
        .ok()?
        .dyn_into()
        .ok()?;
    canvas.set_width(width);
    canvas.set_height(height);
    let context: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
    context
        .draw_image_with_html_video_element_and_dw_and_dh(
            video,
            0.0,
            0.0,
            f64::from(width),
            f64::from(height),
        )
        .ok()?;

    let url = canvas.to_data_url_with_type("image/jpeg").ok()?;
    url.split_once(',').map(|(_, data)| data.to_string())
}

/// Why the browser refused the camera, in words.
fn describe(e: JsValue) -> String {
    let name = js_sys::Reflect::get(&e, &"name".into())
        .ok()
        .and_then(|name| name.as_string())
        .unwrap_or_default();

    match name.as_str() {
        "NotAllowedError" | "SecurityError" => "the camera was not allowed".into(),
        "NotFoundError" | "OverconstrainedError" => "no camera found".into(),
        "NotReadableError" => "the camera is in use by another app".into(),
        _ => format!("the camera cannot be opened ({name})"),
    }
}
