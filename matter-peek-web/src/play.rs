//! Google Play services' Matter commissioning, in the Android app: its `play`
//! plugin (`PlayPlugin`, in Kotlin). Google Play services puts a device on the
//! network, then hands it over to us: the page commissions it onto our fabric
//! over IP, with the passcode it gets, and answers how that went. A device that
//! another Matter app, such as Google Home, shares with us comes with its
//! pairing code, and goes the same way. The other way round, we share a device
//! with another Matter app on the phone, through a commissioning window we
//! opened on it.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::transport::{self, args, invoke, js_error, IpcChannel};

/// A device Google Play services hands over: where it is, and its passcode.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handover {
    pub addr: String,
    pub passcode: u32,
}

/// A device shared with us by another Matter app.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shared {
    /// Its manual pairing code, for the commissioning window opened for us.
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    pub vendor_id: u16,
    pub product_id: u16,
    /// How long its commissioning window stays open, in seconds.
    pub window_secs: i64,
}

#[derive(Deserialize)]
struct Available {
    available: bool,
}

#[derive(Deserialize)]
struct Ended {
    done: bool,
}

/// Whether Google Play services commissions here: in the Android app, with
/// Google Play services on the phone.
pub async fn available() -> bool {
    if !transport::in_app() {
        return false;
    }

    match invoke("plugin:play|available", args(&[])).await {
        Ok(answer) => from_js::<Available>(&answer).is_some_and(|a| a.available),
        Err(_) => false,
    }
}

/// From now on, `on_share` gets the devices shared with us, starting with one
/// shared before the page was up.
pub fn watch_shares(on_share: impl Fn(Shared) + 'static) {
    let channel = IpcChannel::new();
    let onmessage = Closure::<dyn Fn(JsValue)>::new(move |msg: JsValue| {
        if let Some(shared) = from_js::<Shared>(&msg) {
            on_share(shared);
        }
    });
    channel.set_onmessage(onmessage.as_ref().unchecked_ref());
    // The page watches as long as it lives.
    onmessage.forget();

    spawn_local(async move {
        if let Err(e) = invoke(
            "plugin:play|watch_shares",
            args(&[("channel", channel.into())]),
        )
        .await
        {
            log::warn!("play: not watching for shared devices: {}", js_error(e));
        }
    });
}

/// Commission a device with Google Play services: the one `shared` with us, or
/// a new one. `on_handover` gets it once it is on the network. Ends when
/// Google Play services' screens close: whether the device is commissioned.
pub async fn commission(
    shared: Option<&Shared>,
    on_handover: impl Fn(Handover) + 'static,
) -> Result<bool, String> {
    let channel = IpcChannel::new();
    let onmessage = Closure::<dyn Fn(JsValue)>::new(move |msg: JsValue| {
        if let Some(handover) = from_js::<Handover>(&msg) {
            on_handover(handover);
        }
    });
    channel.set_onmessage(onmessage.as_ref().unchecked_ref());

    let mut fields = vec![("handoff", JsValue::from(channel))];
    if let Some(shared) = shared {
        fields.push(("code", shared.code.as_str().into()));
        fields.push(("vendorId", shared.vendor_id.into()));
        fields.push(("productId", shared.product_id.into()));
        if let Some(name) = &shared.name {
            fields.push(("name", name.as_str().into()));
        }
    }

    let ended = invoke("plugin:play|commission", args(&fields)).await;
    drop(onmessage);

    match ended {
        Ok(answer) => Ok(from_js::<Ended>(&answer).is_some_and(|e| e.done)),
        Err(e) => Err(js_error(e)),
    }
}

/// A device to share with another Matter app on the phone, through the
/// commissioning window we opened on it.
pub struct ToShare {
    pub name: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub discriminator: u16,
    pub passcode: u32,
    /// How long the window stays open.
    pub remaining_secs: u64,
}

/// Share `device` with another Matter app on the phone, which the user picks
/// from Android's list of them. Ends when Android's screens close: whether an
/// app was picked (not that it has commissioned the device yet).
pub async fn share(device: ToShare) -> Result<bool, String> {
    let fields = [
        ("name", JsValue::from(device.name)),
        ("vendorId", device.vendor_id.unwrap_or_default().into()),
        ("productId", device.product_id.unwrap_or_default().into()),
        ("discriminator", device.discriminator.into()),
        ("passcode", device.passcode.into()),
        // As a number: the window is open for minutes, not for ages.
        ("remainingSecs", (device.remaining_secs as f64).into()),
    ];

    match invoke("plugin:play|share", args(&fields)).await {
        Ok(answer) => Ok(from_js::<Ended>(&answer).is_some_and(|e| e.done)),
        Err(e) => Err(js_error(e)),
    }
}

/// Tell Google Play services how the commission of the device it handed over
/// went: the device's node ID, or why it failed.
pub fn handed_over(outcome: Result<u64, String>) {
    let fields = match outcome {
        Ok(node_id) => [("nodeId", JsValue::from(node_id.to_string()))],
        Err(error) => [("error", JsValue::from(error))],
    };

    spawn_local(async move {
        if let Err(e) = invoke("plugin:play|handed_over", args(&fields)).await {
            log::warn!("play: the handover's answer did not go: {}", js_error(e));
        }
    });
}

/// `value`, a plugin's answer or message, as a `T`.
fn from_js<T: DeserializeOwned>(value: &JsValue) -> Option<T> {
    let json = match value.as_string() {
        Some(json) => json,
        None => js_sys::JSON::stringify(value).ok()?.as_string()?,
    };

    serde_json::from_str(&json).ok()
}
