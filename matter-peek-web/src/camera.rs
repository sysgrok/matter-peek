//! Reading a QR code with the camera: the browser's video, a frame of which is
//! sent to the server to decode every now and then.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaDevices, MediaStream,
    MediaStreamConstraints, MediaStreamTrack,
};

/// The longest side of a frame sent to decode, in pixels.
const FRAME_MAX: u32 = 1920;

/// The resolution to ask the camera for: a small printed code needs pixels.
const IDEAL_WIDTH: u32 = 3840;
const IDEAL_HEIGHT: u32 = 2160;

/// What of the view a frame takes (its share of the width, of the height):
/// the scanning frame (`.camera-frame`, inset 22% / 14%) and a margin around
/// it, so a code a little off it still reads.
const CROP: (f64, f64) = (0.78, 0.86);

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
    /// The camera's zoom, if it has one.
    pub zoom: Option<Zoom>,
    /// The camera's focus, if it can be set by hand.
    pub focus: Option<Focus>,
    /// The camera refocuses when asked to (a tap on the picture).
    pub can_refocus: bool,
    /// How many times it was asked to, so that the frame shows each.
    pub refocused: u32,
}

/// A camera's zoom: its range, and where it is.
#[derive(Clone, Copy, PartialEq)]
pub struct Zoom {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub value: f64,
}

/// A camera's focus, by distance in meters.
#[derive(Clone, Copy, PartialEq)]
pub struct Focus {
    pub near: f64,
    pub far: f64,
    pub distance: f64,
    /// Set by hand, rather than by the camera.
    pub manual: bool,
}

impl Focus {
    /// Where the distance is on a slider from 0 (near) to 1 (far): even in
    /// diopters, as the lens moves.
    pub fn position(&self) -> f64 {
        let near = 1.0 / self.near;
        ((near - 1.0 / self.distance) / (near - 1.0 / self.far)).clamp(0.0, 1.0)
    }

    /// The distance at `position` on that slider.
    pub fn distance_at(&self, position: f64) -> f64 {
        let near = 1.0 / self.near;
        1.0 / (near + position * (1.0 / self.far - near))
    }
}

impl Camera {
    pub fn new() -> Self {
        Self {
            stream: None,
            running: Rc::new(Cell::new(true)),
            decoding: false,
            error: None,
            zoom: None,
            focus: None,
            can_refocus: false,
            refocused: 0,
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

/// Ask the browser for the camera: on phones, a rear one that focuses by
/// itself.
pub async fn open() -> Result<MediaStream, String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let devices = window.navigator().media_devices().map_err(|_| {
        "this page cannot use the camera: browsers allow it on https or localhost only".to_string()
    })?;

    let first = get(&devices, None).await?;
    if autofocuses(&first) {
        return Ok(first);
    }

    // Phones have several rear cameras, some that do not focus close (an
    // ultra-wide): another one that does. Some phones open one at a time.
    let first_id = device_id(&first);
    stop(&first);
    for id in rear_cameras(&devices).await {
        if Some(&id) == first_id.as_ref() {
            continue;
        }
        if let Ok(stream) = get(&devices, Some(&id)).await {
            if autofocuses(&stream) {
                return Ok(stream);
            }
            stop(&stream);
        }
    }

    get(&devices, first_id.as_deref()).await
}

/// The video of camera `id`, or else of the rear one.
async fn get(devices: &MediaDevices, id: Option<&str>) -> Result<MediaStream, String> {
    let camera = match id {
        Some(id) => ("deviceId", object(&[("exact", id.into())])),
        None => ("facingMode", "environment".into()),
    };
    let video = object(&[
        camera,
        ("width", object(&[("ideal", IDEAL_WIDTH.into())])),
        ("height", object(&[("ideal", IDEAL_HEIGHT.into())])),
        // Refocusing as the code comes closer, where the camera can.
        (
            "advanced",
            js_sys::Array::of1(&object(&[("focusMode", "continuous".into())])).into(),
        ),
    ]);
    let constraints = MediaStreamConstraints::new();
    constraints.set_video(&video);

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(describe)?;
    let stream = JsFuture::from(promise).await.map_err(describe)?;

    Ok(stream.unchecked_into())
}

/// The ids of the rear cameras, as far as the browser tells.
async fn rear_cameras(devices: &MediaDevices) -> Vec<String> {
    let Ok(promise) = devices.enumerate_devices() else {
        return Vec::new();
    };
    let Ok(list) = JsFuture::from(promise).await else {
        return Vec::new();
    };
    let text = |device: &JsValue, name: &str| {
        js_sys::Reflect::get(device, &name.into())
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_default()
    };

    js_sys::Array::from(&list)
        .iter()
        .filter(|device| text(device, "kind") == "videoinput")
        .filter(|device| {
            let facing = call(device, "getCapabilities", &[])
                .and_then(|capabilities| {
                    js_sys::Reflect::get(&capabilities, &"facingMode".into()).ok()
                })
                .is_some_and(|modes| {
                    js_sys::Array::from(&modes)
                        .iter()
                        .any(|mode| mode.as_string().as_deref() == Some("environment"))
                });
            facing || text(device, "label").contains("back")
        })
        .map(|device| text(&device, "deviceId"))
        .filter(|id| !id.is_empty())
        .collect()
}

/// Whether `stream`'s camera focuses by itself.
fn autofocuses(stream: &MediaStream) -> bool {
    capabilities(stream).is_some_and(|(capabilities, _)| {
        focus_modes(&capabilities)
            .iter()
            .any(|mode| mode == "continuous")
    })
}

/// The id of `stream`'s camera.
fn device_id(stream: &MediaStream) -> Option<String> {
    let (_, settings) = capabilities(stream)?;

    js_sys::Reflect::get(&settings, &"deviceId".into())
        .ok()?
        .as_string()
}

/// The zoom of `stream`'s camera, if it can (phones, mostly).
pub fn zoom(stream: &MediaStream) -> Option<Zoom> {
    let (capabilities, settings) = capabilities(stream)?;
    let range = js_sys::Reflect::get(&capabilities, &"zoom".into()).ok()?;
    let (min, max) = (number(&range, "min")?, number(&range, "max")?);

    (max > min).then(|| Zoom {
        min,
        max,
        step: number(&range, "step")
            .filter(|step| *step > 0.0)
            .unwrap_or(0.1),
        value: number(&settings, "zoom").unwrap_or(min),
    })
}

/// The focus of `stream`'s camera, if it can be set by hand.
pub fn focus(stream: &MediaStream) -> Option<Focus> {
    let (capabilities, settings) = capabilities(stream)?;
    if !focus_modes(&capabilities)
        .iter()
        .any(|mode| mode == "manual")
    {
        return None;
    }
    let range = js_sys::Reflect::get(&capabilities, &"focusDistance".into()).ok()?;
    let (near, far) = (number(&range, "min")?, number(&range, "max")?);

    (near > 0.0 && far > near && far.is_finite()).then(|| Focus {
        near,
        far,
        distance: number(&settings, "focusDistance")
            .filter(|distance| *distance > 0.0)
            .unwrap_or(far)
            .clamp(near, far),
        manual: false,
    })
}

/// Whether `stream`'s camera refocuses when asked to.
pub fn can_refocus(stream: &MediaStream) -> bool {
    capabilities(stream).is_some_and(|(capabilities, _)| {
        focus_modes(&capabilities)
            .iter()
            .any(|mode| mode == "single-shot")
    })
}

/// Zoom `stream`'s camera to `value`.
pub fn set_zoom(stream: &MediaStream, value: f64) {
    apply(stream, vec![object(&[("zoom", value.into())])]);
}

/// Focus `stream`'s camera at `distance`, by hand.
pub fn set_focus(stream: &MediaStream, distance: f64) {
    apply(
        stream,
        vec![object(&[
            ("focusMode", "manual".into()),
            ("focusDistance", distance.into()),
        ])],
    );
}

/// Have `stream`'s camera focus on the middle of the picture, where the code
/// goes, as it keeps focusing by itself.
pub fn aim(stream: &MediaStream) {
    apply(stream, vec![object(&[("pointsOfInterest", middle())])]);
}

/// Have `stream`'s camera focus on the middle of the picture once, now.
pub fn refocus(stream: &MediaStream) {
    apply(
        stream,
        vec![
            object(&[("pointsOfInterest", middle())]),
            object(&[("focusMode", "single-shot".into())]),
        ],
    );
}

/// Ask `stream`'s camera for each of `settings` in turn; one it cannot do is
/// passed over.
fn apply(stream: &MediaStream, settings: Vec<JsValue>) {
    let Some(track) = video_track(stream) else {
        return;
    };

    wasm_bindgen_futures::spawn_local(async move {
        for set in settings {
            let constraints = object(&[("advanced", js_sys::Array::of1(&set).into())]);
            if let Some(promise) = call(&track, "applyConstraints", &[constraints]) {
                let _ = JsFuture::from(js_sys::Promise::from(promise)).await;
            }
        }
    });
}

/// What `stream`'s camera can do, and how it is set.
fn capabilities(stream: &MediaStream) -> Option<(JsValue, JsValue)> {
    let track = video_track(stream)?;

    Some((
        call(&track, "getCapabilities", &[])?,
        call(&track, "getSettings", &[])?,
    ))
}

/// The focus modes of a camera of `capabilities`.
fn focus_modes(capabilities: &JsValue) -> Vec<String> {
    js_sys::Reflect::get(capabilities, &"focusMode".into())
        .ok()
        .filter(|modes| modes.is_array())
        .map(|modes| {
            js_sys::Array::from(&modes)
                .iter()
                .filter_map(|mode| mode.as_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The middle of the picture, as a point of interest.
fn middle() -> JsValue {
    js_sys::Array::of1(&object(&[("x", 0.5.into()), ("y", 0.5.into())])).into()
}

/// The number `object.name`, if it has one.
fn number(object: &JsValue, name: &str) -> Option<f64> {
    js_sys::Reflect::get(object, &name.into())
        .ok()
        .and_then(|value| value.as_f64())
}

fn video_track(stream: &MediaStream) -> Option<JsValue> {
    let tracks = stream.get_video_tracks();
    (tracks.length() > 0).then(|| tracks.get(0))
}

/// `object.name(args)`, if it has such a method.
fn call(object: &JsValue, name: &str, args: &[JsValue]) -> Option<JsValue> {
    let method: js_sys::Function = js_sys::Reflect::get(object, &name.into())
        .ok()?
        .dyn_into()
        .ok()?;
    let args: js_sys::Array = args.iter().collect();

    method.apply(object, &args).ok()
}

/// Stop every track of `stream`, turning the camera off.
pub fn stop(stream: &MediaStream) {
    for track in stream.get_tracks().iter() {
        track.unchecked_into::<MediaStreamTrack>().stop();
    }
}

/// What of the current frame of `video` the scanning frame shows (see
/// [`CROP`]), at the camera's resolution up to [`FRAME_MAX`], as a JPEG,
/// base64-encoded; `None` before it plays.
pub fn frame(video: &HtmlVideoElement) -> Option<String> {
    let (vw, vh) = (
        f64::from(video.video_width()),
        f64::from(video.video_height()),
    );
    if vw == 0.0 || vh == 0.0 {
        return None;
    }

    // The video covers the view (`object-fit: cover`): the part of it the
    // view shows, then the middle of that.
    let (cw, ch) = (
        f64::from(video.client_width()),
        f64::from(video.client_height()),
    );
    let (sx, sy, sw, sh) = if cw > 0.0 && ch > 0.0 {
        let scale = (cw / vw).max(ch / vh);
        let (shown_w, shown_h) = (cw / scale, ch / scale);
        let (sw, sh) = (shown_w * CROP.0, shown_h * CROP.1);
        ((vw - sw) / 2.0, (vh - sh) / 2.0, sw, sh)
    } else {
        (0.0, 0.0, vw, vh)
    };

    let scale = (f64::from(FRAME_MAX) / sw.max(sh)).min(1.0);
    let (width, height) = ((sw * scale) as u32, (sh * scale) as u32);

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
        .draw_image_with_html_video_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            video,
            sx,
            sy,
            sw,
            sh,
            0.0,
            0.0,
            f64::from(width),
            f64::from(height),
        )
        .ok()?;

    let url = canvas.to_data_url_with_type("image/jpeg").ok()?;
    url.split_once(',').map(|(_, data)| data.to_string())
}

/// An object of `fields`.
fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let object = js_sys::Object::new();
    for (name, value) in fields {
        let _ = js_sys::Reflect::set(&object, &JsValue::from_str(name), value);
    }

    object.into()
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
