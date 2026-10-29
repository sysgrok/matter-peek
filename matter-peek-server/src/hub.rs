//! A controller and its assistant, for the connections to them: the server's
//! WebSockets, a process's own (the CLI without a server), an app's. A
//! connection's requests go to the controller, the `Chat…` and `…Assistant…`
//! ones to the assistant, whose chats all connections share, the
//! `…Store` ones to the store's files.

use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use matter_peek_core::api::agent::ModelConfig;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::wire::{ClientMsg, ServerMsg, WireSession};
use matter_peek_core::api::{MatterPeek, PeekConfig};

use crate::chats::Chats;
use crate::store::{StoreFiles, CHATS_DIR};
use crate::threads::Threads;

/// A controller and its assistant.
pub struct Hub {
    peek: Arc<MatterPeek>,
    chats: Rc<Chats>,
    files: Rc<StoreFiles>,
}

/// A connection to a [`Hub`].
pub struct Session {
    wire: Rc<WireSession>,
    chats: Rc<Chats>,
    files: Rc<StoreFiles>,
    out: async_channel::Sender<ServerMsg>,
}

impl Hub {
    /// Start a controller as `config` says, with the assistant's model `env`
    /// sets, else the stored one, and its chats kept next to the store.
    /// `restart`: the process restarts after a store is imported, with it
    /// (a server); else it is used from the next start. Fails if another
    /// matter-peek process holds the store.
    pub fn start(
        config: PeekConfig,
        env: Option<ModelConfig>,
        restart: bool,
    ) -> io::Result<Rc<Self>> {
        let dir = config
            .store_dir
            .as_ref()
            .map_or_else(data_dir, PathBuf::from);
        let peek = MatterPeek::start_with(config, Threads).map_err(io::Error::other)?;
        let chats = Chats::new(peek.clone(), env, dir.join(CHATS_DIR));
        let files = Rc::new(StoreFiles::new(peek.clone(), dir, restart));

        Ok(Rc::new(Self { peek, chats, files }))
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
            files: self.files.clone(),
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
        } else if msg.request.is_store_file() {
            let files = self.files.clone();
            let out = self.out.clone();
            crate::spawn(async move { files.handle(msg, out).await });
        } else {
            let wire = self.wire.clone();
            crate::spawn(async move { wire.handle(msg).await });
        }
    }
}
