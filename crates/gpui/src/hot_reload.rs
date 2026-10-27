use crate::{App, AppContext as _, Global, Task};
use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::Message;
use dioxus_devtools_types::DevserverMsg;
use futures::StreamExt;
use std::net::SocketAddr;
use subsecond::JumpTable;

struct HotReload {
    _task: Task<()>,
}

impl Global for HotReload {}

pub(crate) fn init(cx: &mut App) {
    if cx.has_global::<HotReload>() {
        return;
    }
    let (Some(address), Some(endpoint)) = (
        dioxus_cli_config::devserver_raw_addr(),
        dioxus_cli_config::devserver_ws_endpoint(),
    ) else {
        log::warn!(
            "GPUI hot reload is enabled, but no dx development server was found. \
             Launch the app with `dx serve --hot-patch` to enable hot reloading."
        );
        return;
    };

    let build_id = dioxus_cli_config::build_id();
    let process_id = std::process::id();
    // Resolve the executable anchor on the foreground thread before any patch can load.
    let reference = subsecond::aslr_reference();
    let endpoint =
        format!("{endpoint}?aslr_reference={reference}&build_id={build_id}&pid={process_id}");
    let (sender, receiver) = async_channel::bounded(16);
    let connection = cx.background_spawn(async move {
        if let Err(error) = receive_patches(address, endpoint, build_id, process_id, sender).await {
            log::error!("GPUI hot reload disconnected: {error:#}. Restart dx serve to reconnect.");
        }
    });
    let task = cx.spawn(async move |cx| {
        let _connection = connection;
        while let Ok(table) = receiver.recv().await {
            cx.update(|cx| {
                // Patches are executable code from the developer's trusted dx server. Applying
                // them between foreground updates avoids replacing code during a render.
                match unsafe { subsecond::apply_patch(table) } {
                    Ok(()) => {
                        cx.refresh_windows();
                        log::info!("GPUI hot reload applied");
                    }
                    Err(error) => log::error!("GPUI hot reload failed: {error}"),
                }
            });
        }
    });
    cx.set_global(HotReload { _task: task });
}

async fn receive_patches(
    address: SocketAddr,
    endpoint: String,
    build_id: u64,
    process_id: u32,
    sender: async_channel::Sender<JumpTable>,
) -> Result<()> {
    let stream = smol::net::TcpStream::connect(address)
        .await
        .context("connecting to the dx development server")?;
    let (mut websocket, _) = async_tungstenite::client_async(endpoint, stream)
        .await
        .context("opening the dx development websocket")?;
    log::info!("GPUI hot reload connected");

    while let Some(message) = websocket.next().await {
        match message.context("reading the dx development websocket")? {
            Message::Text(text) => {
                let message = serde_json::from_str::<DevserverMsg>(&text)
                    .context("decoding a dx development message")?;
                if let Some(table) = patch_for_process(message, build_id, process_id) {
                    if sender.send(table).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    log::info!("GPUI hot reload disconnected");
    Ok(())
}

fn patch_for_process(message: DevserverMsg, build_id: u64, process_id: u32) -> Option<JumpTable> {
    match message {
        DevserverMsg::HotReload(message)
            if message.for_build_id == Some(build_id) && message.for_pid == Some(process_id) =>
        {
            message.jump_table
        }
        DevserverMsg::FullReloadCommand => {
            log::warn!("GPUI hot reload requires a full application restart");
            None
        }
        _ => None,
    }
}

