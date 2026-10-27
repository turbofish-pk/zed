pub mod user;

use anyhow::Result;
use futures::{FutureExt, Stream, TryFutureExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{App, AsyncApp, Entity, Global, WeakEntity};
use http_client::{HttpClientWithUrl, read_proxy_from_env};
use parking_lot::Mutex;
use postage::watch;
use rpc::proto::{EnvelopedMessage, PeerId, RequestMessage};
pub use rpc::*;
use serde::Deserialize;
use settings::{RegisterSetting, Settings};
use std::{
    any::TypeId,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use url::Url;
pub use user::*;

static ZED_SERVER_URL: LazyLock<Option<String>> =
    LazyLock::new(|| std::env::var("ZED_SERVER_URL").ok());

pub static ZED_APP_PATH: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| std::env::var("ZED_APP_PATH").ok().map(PathBuf::from));

pub static ZED_ALWAYS_ACTIVE: LazyLock<bool> =
    LazyLock::new(|| std::env::var("ZED_ALWAYS_ACTIVE").is_ok_and(|e| !e.is_empty()));

#[derive(Deserialize, RegisterSetting)]
pub struct ClientSettings {
    pub server_url: String,
}

impl Settings for ClientSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let server_url = match &*ZED_SERVER_URL {
            Some(url) => url.clone(),
            None => content.server_url.clone().unwrap(),
        };
        Self { server_url }
    }
}

#[derive(Deserialize, Default, RegisterSetting)]
pub struct ProxySettings {
    pub proxy: Option<String>,
}

impl ProxySettings {
    pub fn proxy_url(&self) -> Option<Url> {
        self.proxy
            .as_deref()
            .map(str::trim)
            .filter(|input| !input.is_empty())
            .and_then(|input| {
                input
                    .parse::<Url>()
                    .inspect_err(|e| log::error!("Error parsing proxy settings: {}", e))
                    .ok()
            })
            .or_else(read_proxy_from_env)
    }
}

impl Settings for ProxySettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            proxy: content
                .proxy
                .as_deref()
                .map(str::trim)
                .filter(|proxy| !proxy.is_empty())
                .map(ToOwned::to_owned),
        }
    }
}

struct GlobalClient(Arc<Client>);

impl Global for GlobalClient {}

/// The RPC client, without any way to sign in or connect.
///
/// The status is always `SignedOut`, so every request fails with "not connected".
/// The type and its RPC methods stay because other crates hold a `Client`.
pub struct Client {
    id: AtomicU64,
    peer: Arc<Peer>,
    http: Arc<HttpClientWithUrl>,
    _status_tx: watch::Sender<Status>,
    status_rx: watch::Receiver<Status>,
    handler_set: Mutex<ProtoMessageHandlerSet>,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Status {
    SignedOut,
    UpgradeRequired,
    Authenticating,
    Authenticated,
    AuthenticationError,
    Connecting,
    ConnectionError,
    Connected {
        peer_id: PeerId,
        connection_id: ConnectionId,
    },
    ConnectionLost,
    Reauthenticating,
    Reauthenticated,
    Reconnecting,
    ReconnectionError {
        next_reconnection: Instant,
    },
}

impl Status {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn was_connected(&self) -> bool {
        matches!(
            self,
            Self::ConnectionLost
                | Self::Reauthenticating
                | Self::Reauthenticated
                | Self::Reconnecting
        )
    }

    /// Returns whether the client is currently connected or was connected at some point.
    pub fn is_or_was_connected(&self) -> bool {
        self.is_connected() || self.was_connected()
    }

    pub fn is_signing_in(&self) -> bool {
        matches!(
            self,
            Self::Authenticating | Self::Reauthenticating | Self::Connecting | Self::Reconnecting
        )
    }

    pub fn is_signed_out(&self) -> bool {
        matches!(self, Self::SignedOut | Self::UpgradeRequired)
    }
}

/// Removes its message handler from the client when dropped.
pub struct Subscription {
    client: Weak<Client>,
    id: TypeId,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(client) = self.client.upgrade() {
            let mut handlers = client.handler_set.lock();
            handlers.entities_by_message_type.remove(&self.id);
            handlers.message_handlers.remove(&self.id);
        }
    }
}

impl Client {
    pub fn new(http: Arc<HttpClientWithUrl>) -> Arc<Self> {
        let (status_tx, status_rx) = watch::channel_with(Status::SignedOut);
        Arc::new(Self {
            id: AtomicU64::new(0),
            peer: Peer::new(0),
            http,
            _status_tx: status_tx,
            status_rx,
            handler_set: Default::default(),
        })
    }

    pub fn production(cx: &mut App) -> Arc<Self> {
        let http = Arc::new(HttpClientWithUrl::new_url(
            cx.http_client(),
            &ClientSettings::get_global(cx).server_url,
            cx.http_client().proxy().cloned(),
        ));
        Self::new(http)
    }

    pub fn id(&self) -> u64 {
        self.id.load(Ordering::SeqCst)
    }

    pub fn http_client(&self) -> Arc<HttpClientWithUrl> {
        self.http.clone()
    }

    pub fn global(cx: &App) -> Arc<Self> {
        cx.global::<GlobalClient>().0.clone()
    }

    pub fn set_global(client: Arc<Client>, cx: &mut App) {
        cx.set_global(GlobalClient(client))
    }

    /// Always `None`: this build never signs in.
    pub fn user_id(&self) -> Option<u64> {
        None
    }

    pub fn peer_id(&self) -> Option<PeerId> {
        if let Status::Connected { peer_id, .. } = &*self.status().borrow() {
            Some(*peer_id)
        } else {
            None
        }
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status_rx.clone()
    }

    #[track_caller]
    pub fn add_message_handler<M, E, H, F>(
        self: &Arc<Self>,
        entity: WeakEntity<E>,
        handler: H,
    ) -> Subscription
    where
        M: EnvelopedMessage,
        E: 'static,
        H: 'static + Sync + Fn(Entity<E>, TypedEnvelope<M>, AsyncApp) -> F + Send + Sync,
        F: 'static + Future<Output = Result<()>>,
    {
        self.add_message_handler_impl(entity, move |entity, message, _, cx| {
            handler(entity, message, cx)
        })
    }

    #[track_caller]
    fn add_message_handler_impl<M, E, H, F>(
        self: &Arc<Self>,
        entity: WeakEntity<E>,
        handler: H,
    ) -> Subscription
    where
        M: EnvelopedMessage,
        E: 'static,
        H: 'static
            + Sync
            + Fn(Entity<E>, TypedEnvelope<M>, AnyProtoClient, AsyncApp) -> F
            + Send
            + Sync,
        F: 'static + Future<Output = Result<()>>,
    {
        let message_type_id = TypeId::of::<M>();
        let mut state = self.handler_set.lock();
        state
            .entities_by_message_type
            .insert(message_type_id, entity.into());

        let prev_handler = state.message_handlers.insert(
            message_type_id,
            Arc::new(move |subscriber, envelope, client, cx| {
                let subscriber = subscriber.downcast::<E>().unwrap();
                let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                handler(subscriber, *envelope, client, cx).boxed_local()
            }),
        );
        if prev_handler.is_some() {
            let location = std::panic::Location::caller();
            panic!(
                "{}:{} registered handler for the same message {} twice",
                location.file(),
                location.line(),
                std::any::type_name::<M>()
            );
        }

        Subscription {
            client: Arc::downgrade(self),
            id: message_type_id,
        }
    }

    #[track_caller]
    pub fn add_request_handler<M, E, H, F>(
        self: &Arc<Self>,
        entity: WeakEntity<E>,
        handler: H,
    ) -> Subscription
    where
        M: RequestMessage,
        E: 'static,
        H: 'static + Sync + Fn(Entity<E>, TypedEnvelope<M>, AsyncApp) -> F + Send + Sync,
        F: 'static + Future<Output = Result<M::Response>>,
    {
        self.add_message_handler_impl(entity, move |handle, envelope, this, cx| {
            Self::respond_to_request(envelope.receipt(), handler(handle, envelope, cx), this)
        })
    }

    async fn respond_to_request<T: RequestMessage, F: Future<Output = Result<T::Response>>>(
        receipt: Receipt<T>,
        response: F,
        client: AnyProtoClient,
    ) -> Result<()> {
        match response.await {
            Ok(response) => {
                client.send_response(receipt.message_id, response)?;
                Ok(())
            }
            Err(error) => {
                client.send_response(receipt.message_id, error.to_proto())?;
                Err(error)
            }
        }
    }

    pub fn disconnect(&self, _cx: &AsyncApp) {
        self.peer.teardown();
    }

    fn connection_id(&self) -> Result<ConnectionId> {
        if let Status::Connected { connection_id, .. } = *self.status().borrow() {
            Ok(connection_id)
        } else {
            anyhow::bail!("not connected");
        }
    }

    pub fn send<T: EnvelopedMessage>(&self, message: T) -> Result<()> {
        self.peer.send(self.connection_id()?, message)
    }

    pub fn request<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> + use<T> {
        self.request_envelope(request)
            .map_ok(|envelope| envelope.payload)
    }

    pub fn request_stream<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<impl Stream<Item = Result<T::Response>>>> {
        let response = self
            .connection_id()
            .map(|conn_id| self.peer.request_stream(conn_id, request));
        async move { response?.await }
    }

    pub fn request_envelope<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<TypedEnvelope<T::Response>>> + use<T> {
        let response = self
            .connection_id()
            .map(|conn_id| self.peer.request_envelope(conn_id, request));
        async move { response?.await }
    }

    pub fn request_dynamic(
        &self,
        envelope: proto::Envelope,
        request_type: &'static str,
    ) -> impl Future<Output = Result<proto::Envelope>> + use<> {
        let response = self
            .connection_id()
            .map(|conn_id| self.peer.request_dynamic(conn_id, envelope, request_type));
        async move { Ok(response?.await?.0) }
    }
}

impl ProtoClient for Client {
    fn request(
        &self,
        envelope: proto::Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<proto::Envelope>> {
        self.request_dynamic(envelope, request_type).boxed()
    }

    fn request_stream(
        &self,
        envelope: proto::Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<proto::Envelope>>>> {
        let response = self.connection_id().map(|connection_id| {
            self.peer
                .request_stream_dynamic(connection_id, envelope, request_type)
        });
        async move { response?.await }.boxed()
    }

    fn send(&self, envelope: proto::Envelope, _message_type: &'static str) -> Result<()> {
        let connection_id = self.connection_id()?;
        self.peer.send_dynamic(connection_id, envelope)
    }

    fn send_response(&self, envelope: proto::Envelope, _message_type: &'static str) -> Result<()> {
        let connection_id = self.connection_id()?;
        self.peer.send_dynamic(connection_id, envelope)
    }

    fn message_handler_set(&self) -> &Mutex<ProtoMessageHandlerSet> {
        &self.handler_set
    }

    fn is_via_collab(&self) -> bool {
        true
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

/// prefix for the zed:// url scheme
pub const ZED_URL_SCHEME: &str = "zed";

/// A parsed Zed link that can be handled internally by the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZedLink {
    /// Join a channel: `zed.dev/channel/channel-name-123` or `zed://channel/channel-name-123`
    Channel { channel_id: u64 },
    /// Open channel notes: `zed.dev/channel/channel-name-123/notes` or with heading `notes#heading`
    ChannelNotes {
        channel_id: u64,
        heading: Option<String>,
    },
}

/// Parses the given link into a Zed link.
///
/// Returns a [`Some`] containing the parsed link if the link is a recognized Zed link
/// that should be handled internally by the application.
/// Returns [`None`] for links that should be opened in the browser.
pub fn parse_zed_link(link: &str, cx: &App) -> Option<ZedLink> {
    let server_url = &ClientSettings::get_global(cx).server_url;
    let path = link
        .strip_prefix(server_url)
        .and_then(|result| result.strip_prefix('/'))
        .or_else(|| {
            link.strip_prefix(ZED_URL_SCHEME)
                .and_then(|result| result.strip_prefix("://"))
        })?;

    let mut parts = path.split('/');

    if parts.next() != Some("channel") {
        return None;
    }

    let slug = parts.next()?;
    let id_str = slug.split('-').next_back()?;
    let channel_id = id_str.parse::<u64>().ok()?;

    let Some(next) = parts.next() else {
        return Some(ZedLink::Channel { channel_id });
    };

    if let Some(heading) = next.strip_prefix("notes#") {
        return Some(ZedLink::ChannelNotes {
            channel_id,
            heading: Some(heading.to_string()),
        });
    }

    if next == "notes" {
        return Some(ZedLink::ChannelNotes {
            channel_id,
            heading: None,
        });
    }

    None
}
