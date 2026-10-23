//! [`WireSession`]: the wire messages, run against a [`MatterPeek`].

use core::cell::RefCell;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::api::peek::{
    CommissionListener, HealthListener, MatterPeek, ModelListener, OtaListener, OtaUpload,
    Subscription,
};
use crate::api::types::*;

use super::{ClientMsg, Event, ReqId, Request, Response, ServerMsg};

/// One client's conversation with a [`MatterPeek`] in [`ClientMsg`]s and
/// [`ServerMsg`]s: it runs the requests, and holds what outlives them (the
/// client's subscriptions and uploads, which end with the session).
///
/// Its requests may run concurrently (a `Cancel` during a `Commission`, say):
/// spawn each [`WireSession::handle`].
pub struct WireSession {
    peek: Arc<MatterPeek>,
    out: async_channel::Sender<ServerMsg>,
    subscriptions: RefCell<BTreeMap<u64, Arc<Subscription>>>,
    model_subscriptions: RefCell<BTreeMap<u64, (ModelScope, Arc<Subscription>)>>,
    uploads: RefCell<BTreeMap<u32, Arc<OtaUpload>>>,
}

impl WireSession {
    /// A session with `peek`, whose responses and events go to `out`.
    pub fn new(peek: Arc<MatterPeek>, out: async_channel::Sender<ServerMsg>) -> Self {
        Self {
            peek,
            out,
            subscriptions: RefCell::new(BTreeMap::new()),
            model_subscriptions: RefCell::new(BTreeMap::new()),
            uploads: RefCell::new(BTreeMap::new()),
        }
    }

    /// Run the request of `msg`, sending its events and then its response.
    pub async fn handle(&self, msg: ClientMsg) {
        let response = self.run(msg.req_id, msg.request).await;

        let _ = self
            .out
            .send(ServerMsg::Response {
                req_id: msg.req_id,
                response,
            })
            .await;
    }

    /// Run `request`, and answer, without its `req_id`: for a caller in this
    /// process, such as an agent's tools. Its events still go out.
    pub async fn answer(&self, request: Request) -> Response {
        self.run(0, request).await
    }

    async fn run(&self, req_id: ReqId, request: Request) -> Response {
        let peek = &self.peek;
        let out = &self.out;

        let response = match request {
            Request::Commission {
                via,
                setup_code,
                remember,
                fabric,
            } => {
                let steps = Arc::new(Steps {
                    req_id,
                    out: out.clone(),
                });
                peek.commission(via, setup_code, remember, fabric, Some(steps))
                    .await
                    .map(Response::Commissioned)
            }
            Request::Fabrics => peek.fabrics().await.map(fabrics),
            Request::CreateFabric { name } => peek.create_fabric(name).await.map(fabrics),
            Request::RenameFabric { fabric, name } => {
                peek.rename_fabric(fabric, name).await.map(fabrics)
            }
            Request::SetDefaultFabric { fabric } => {
                peek.set_default_fabric(fabric).await.map(fabrics)
            }
            Request::DeleteFabric { fabric, deletion } => {
                peek.delete_fabric(fabric, deletion).await.map(fabrics)
            }
            Request::ReadQrImage { image } => match BASE64.decode(image) {
                Ok(image) => peek
                    .read_qr_image(image)
                    .await
                    .map(|setup_code| Response::SetupCode { setup_code }),
                Err(e) => Err(PeekError::failed(alloc::format!(
                    "the image is not base64: {e}"
                ))),
            },
            Request::Discover {
                discriminator,
                setup_code,
                max,
            } => peek
                .discover(discriminator, setup_code, max)
                .await
                .map(|found| Response::Discovered {
                    devices: found.devices,
                    ble_devices: found.ble_devices,
                }),
            Request::ReadModel {
                node_id,
                endpoint,
                cluster,
            } => peek
                .read_model(node_id, endpoint, cluster)
                .await
                .map(|model| Response::Model { model }),
            Request::ReadEvents {
                node_id,
                endpoint,
                cluster,
            } => peek
                .read_events(node_id, endpoint, cluster)
                .await
                .map(|events| Response::Events { events }),
            Request::WriteAttribute {
                node_id,
                endpoint,
                cluster,
                attribute,
                value,
            } => peek
                .write_attribute(node_id, endpoint, cluster, attribute, value)
                .await
                .map(|model| Response::Model { model }),
            Request::InvokeCommand {
                node_id,
                endpoint,
                cluster,
                command,
                fields,
            } => peek
                .invoke_command(node_id, endpoint, cluster, command, fields)
                .await
                .map(|outcome| Response::CommandOutcome { outcome }),
            Request::AdvertisedDevices => peek
                .advertised_devices()
                .await
                .map(|devices| Response::AdvertisedDevices { devices }),
            Request::ListDevices { fabric } => peek.list_devices(fabric).await.map(devices),
            Request::PeekDevice { node_id } => peek
                .peek_device(node_id)
                .await
                .map(|health| Response::Peek { node_id, health }),
            Request::RemoveDeviceFabric {
                node_id,
                fabric_index,
            } => peek
                .remove_device_fabric(node_id, fabric_index)
                .await
                .map(|health| Response::Peek { node_id, health }),
            Request::ReadNetwork { node_id } => {
                peek.read_network(node_id)
                    .await
                    .map(|network| Response::Network {
                        node_id,
                        network: Box::new(network),
                    })
            }
            Request::ChangeNetwork {
                node_id,
                endpoint,
                change,
            } => peek
                .change_network(node_id, endpoint, change)
                .await
                .map(|network| Response::Network {
                    node_id,
                    network: Box::new(network),
                }),
            Request::RetrieveLogs { node_id, intent } => peek
                .retrieve_logs(node_id, intent)
                .await
                .map(|log| Response::Logs {
                    node_id,
                    intent,
                    data: BASE64.encode(log),
                }),
            Request::Identify { node_id, secs } => {
                peek.identify(node_id, secs).await.map(|()| Response::Ok)
            }
            Request::OpenCommissioningWindow {
                node_id,
                timeout_secs,
            } => peek
                .open_commissioning_window(node_id, timeout_secs)
                .await
                .map(window),
            Request::CloseCommissioningWindow { node_id } => {
                peek.close_commissioning_window(node_id).await.map(window)
            }
            Request::CommissioningWindow { node_id } => {
                peek.commissioning_window(node_id).await.map(window)
            }
            Request::DeviceTime { node_id } => peek
                .device_time(node_id)
                .await
                .map(|time| Response::DeviceTime { time }),
            Request::SetDeviceTime { node_id } => peek
                .set_device_time(node_id)
                .await
                .map(|setup| Response::TimeSetup { setup }),
            Request::Subscribe { node_id } => self.subscribe(node_id).await,
            Request::Unsubscribe { node_id } => {
                // Dropping it stops it.
                self.subscriptions.borrow_mut().remove(&node_id);
                Ok(Response::Ok)
            }
            Request::SubscribeModel { node_id, scope } => {
                self.subscribe_model(node_id, scope).await
            }
            Request::UnsubscribeModel { node_id } => {
                self.model_subscriptions.borrow_mut().remove(&node_id);
                Ok(Response::Ok)
            }
            Request::Decommission { node_id, force } => {
                peek.decommission(node_id, force).await.map(devices)
            }
            Request::Sync { fabric } => peek.sync(fabric).await.map(devices),
            Request::Remember { node_id } => peek.remember(node_id).await.map(devices),
            Request::SetTags { node_id, tags } => peek.set_tags(node_id, tags).await.map(devices),
            Request::Cancel => {
                peek.cancel_commission();
                Ok(Response::Ok)
            }
            Request::OtaImages => peek.ota_images().await.map(images),
            Request::OtaUpdates => peek
                .ota_updates()
                .await
                .map(|updates| Response::OtaUpdates { updates }),
            Request::OtaUploadBegin { size } => peek.ota_upload(size).await.map(|upload| {
                let upload_id = upload.id();
                self.uploads.borrow_mut().insert(upload_id, upload);

                Response::OtaUploadStarted { upload_id }
            }),
            Request::OtaUploadChunk {
                upload_id,
                offset,
                data,
            } => self.upload_chunk(upload_id, offset, &data).await,
            Request::OtaUploadEnd { upload_id } => {
                let upload = self.uploads.borrow_mut().remove(&upload_id);
                match upload {
                    Some(upload) => upload.finish().await.map(images),
                    None => Err(no_upload(upload_id)),
                }
            }
            Request::OtaDelete { name } => peek.ota_delete(name).await.map(images),
            Request::OtaUpdate { node_id, name } => {
                let progress = Arc::new(Progress { out: out.clone() });
                peek.ota_update(node_id, name, Some(progress))
                    .await
                    .map(|()| Response::Ok)
            }
            Request::ReadThreadMesh { fabric } => {
                peek.read_thread_mesh(fabric)
                    .await
                    .map(|mesh| Response::ThreadMesh {
                        mesh: Box::new(mesh),
                    })
            }
            Request::Assistant => peek.assistant().await.map(assistant),
            Request::SetAssistant { update } => peek
                .set_assistant(update)
                .await
                .map(|info| assistant(Some(info))),
            Request::ClearAssistant => peek.clear_assistant().await.map(|()| assistant(None)),
            Request::ChatList
            | Request::ChatOpen { .. }
            | Request::ChatAsk { .. }
            | Request::ChatApprove { .. }
            | Request::ChatStop { .. }
            | Request::ChatDelete { .. }
            | Request::AssistantModels { .. }
            | Request::AssistantTest { .. } => Err(PeekError::failed(
                "the assistant runs in the server (matter-peek serve), not in this process",
            )),
        };

        response.unwrap_or_else(|e| match e {
            PeekError::Cancelled { decommissioned } => Response::Cancelled { decommissioned },
            e => Response::Error {
                message: e.to_string(),
            },
        })
    }

    async fn subscribe(&self, node_id: u64) -> Result<Response, PeekError> {
        if self.subscriptions.borrow().contains_key(&node_id) {
            return Ok(Response::Subscribed {
                node_id,
                already: true,
            });
        }

        let reports = Arc::new(Reports {
            out: self.out.clone(),
        });
        let subscription = self.peek.subscribe(node_id, reports).await?;
        self.subscriptions
            .borrow_mut()
            .insert(node_id, subscription);

        Ok(Response::Subscribed {
            node_id,
            already: false,
        })
    }

    async fn subscribe_model(
        &self,
        node_id: u64,
        scope: ModelScope,
    ) -> Result<Response, PeekError> {
        let already = self
            .model_subscriptions
            .borrow()
            .get(&node_id)
            .is_some_and(|(followed, _)| *followed == scope);
        if already {
            return Ok(Response::Subscribed {
                node_id,
                already: true,
            });
        }

        let listener = Arc::new(Reports {
            out: self.out.clone(),
        });
        let subscription = self.peek.subscribe_model(node_id, scope, listener).await?;
        // Replaced once the new one runs, so that the coverage never dips.
        self.model_subscriptions
            .borrow_mut()
            .insert(node_id, (scope, subscription));

        Ok(Response::Subscribed {
            node_id,
            already: false,
        })
    }

    async fn upload_chunk(
        &self,
        upload_id: u32,
        offset: u64,
        data: &str,
    ) -> Result<Response, PeekError> {
        let upload = self.uploads.borrow().get(&upload_id).cloned();
        let Some(upload) = upload else {
            return Err(no_upload(upload_id));
        };

        let written = match BASE64.decode(data) {
            Ok(chunk) => upload.write(offset, chunk).await,
            Err(e) => Err(PeekError::failed(alloc::format!(
                "a chunk is not base64: {e}"
            ))),
        };

        // A failed chunk fails the upload.
        if written.is_err() {
            self.uploads.borrow_mut().remove(&upload_id);
        }

        written.map(|()| Response::Ok)
    }
}

fn devices(list: DeviceList) -> Response {
    Response::Devices {
        devices: list.devices,
        notice: list.notice,
    }
}

fn fabrics(fabrics: Vec<FabricInfo>) -> Response {
    Response::Fabrics { fabrics }
}

fn window(window: CommissioningWindow) -> Response {
    Response::CommissioningWindow { window }
}

fn images(images: Vec<OtaImageInfo>) -> Response {
    Response::OtaImages { images }
}

fn assistant(assistant: Option<AssistantInfo>) -> Response {
    Response::Assistant { assistant }
}

fn no_upload(upload_id: u32) -> PeekError {
    PeekError::failed(alloc::format!("no upload {upload_id}"))
}

/// A commission's steps, as events.
struct Steps {
    req_id: ReqId,
    out: async_channel::Sender<ServerMsg>,
}

impl CommissionListener for Steps {
    fn on_step(&self, step: CommissionStep, status: StepStatus) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::Step {
                req_id: self.req_id,
                step,
                status,
            },
        });
    }
}

/// A subscription's reports, as events.
struct Reports {
    out: async_channel::Sender<ServerMsg>,
}

impl ModelListener for Reports {
    fn on_model(&self, node_id: u64, model: DeviceModel) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::Model { node_id, model },
        });
    }
}

impl HealthListener for Reports {
    fn on_report(&self, node_id: u64, seq: u64, health: DeviceHealth) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::HealthReport {
                node_id,
                seq,
                health,
            },
        });
    }

    fn on_state(&self, node_id: u64, state: SubscriptionState) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::SubscriptionState { node_id, state },
        });
    }
}

/// An update's progress, as events.
struct Progress {
    out: async_channel::Sender<ServerMsg>,
}

impl OtaListener for Progress {
    fn on_progress(&self, node_id: u64, progress: OtaProgress) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::OtaProgress { node_id, progress },
        });
    }
}
