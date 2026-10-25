//! A controller and its assistant, for the connections to them: the server's
//! WebSockets, a process's own (the CLI without a server), an app's. A
//! connection's requests go to the controller, the `Chat…` and `…Assistant…`
//! ones to the assistant, whose chats all connections share.

use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use matter_peek_core::api::agent::ModelConfig;
use matter_peek_core::api::wire::{ClientMsg, ServerMsg, WireSession};
use matter_peek_core::api::{MatterPeek, PeekConfig};

use crate::chats::Chats;

/// A controller and its assistant.
pub struct Hub {
    peek: Arc<MatterPeek>,
    chats: Rc<Chats>,
}

/// A connection to a [`Hub`].
pub struct Session {
    wire: Rc<WireSession>,
    chats: Rc<Chats>,
    out: async_channel::Sender<ServerMsg>,
}

impl Hub {
    /// Start a controller as `config` says, with the assistant's model `env`
    /// sets, else the stored one, and its chats kept in `chats_dir`. Fails if
    /// another matter-peek process holds the store.
    pub fn start(
        config: PeekConfig,
        env: Option<ModelConfig>,
        chats_dir: PathBuf,
    ) -> io::Result<Rc<Self>> {
        let peek = MatterPeek::start(config).map_err(io::Error::other)?;
        let chats = Chats::new(peek.clone(), env, chats_dir);

        Ok(Rc::new(Self { peek, chats }))
    }

    /// The controller.
    pub fn peek(&self) -> &Arc<MatterPeek> {
        &self.peek
    }

    /// A connection, whose answers and events go to `out`. Its subscriptions
    /// and uploads end when it is dropped.
    pub fn connect(&self, out: async_channel::Sender<ServerMsg>) -> Session {
        Session {
            wire: Rc::new(WireSession::new(self.peek.clone(), out.clone())),
            chats: self.chats.clone(),
            out,
        }
    }
}

impl Session {
    /// Run the request of `msg`, side by side with the others (a `Cancel`
    /// reaches a running `Commission`): on the executor of [`crate::block_on`].
    pub fn handle(&self, msg: ClientMsg) {
        if msg.request.is_assistant() {
            let chats = self.chats.clone();
            let out = self.out.clone();
            crate::spawn(async move { chats.handle(msg, out).await });
        } else {
            let wire = self.wire.clone();
            crate::spawn(async move { wire.handle(msg).await });
        }
    }
}
