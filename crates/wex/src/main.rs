use core::iter;
use core::pin::pin;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use clap::Parser;
use config::Config;
use futures::future::try_join_all;
use futures::{stream, StreamExt as _};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;
use tokio::{fs, select, signal};
use tracing::{debug, error, instrument};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use url::Url;
use wasi_preview1_component_adapter_provider::{
    WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
};
use wasmtime::component::{Component, InstancePre, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

mod config;
mod keyvalue;
mod messaging;

pub mod bindings {
    wasmtime::component::bindgen!({
        world: "imports",
        async: true,
        tracing: true,
        trappable_imports: true,
        with: {
            "wasi:messaging/types/message": crate::messaging::Message,
        },
    });
}

/// Serve a reactor component
#[derive(Parser, Debug)]
pub struct Args {
    /// Path or URL to Wasm command component
    workload: String,
}

pub enum Workload {
    Url(Url),
    Binary(Vec<u8>),
}

pub struct Ctx {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub nats_addr: Arc<str>,
    pub nats_once: OnceCell<async_nats::Client>,
}

fn nats_connect_options() -> async_nats::ConnectOptions {
    async_nats::ConnectOptions::new().retry_on_initial_connect()
}

async fn nats_client(
    once: &OnceCell<async_nats::Client>,
    addr: impl async_nats::ToServerAddrs,
) -> anyhow::Result<&async_nats::Client> {
    once.get_or_try_init(|| nats_connect_options().connect(addr))
        .await
        .context("failed to connect to NATS.io")
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiHttpView for Ctx {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

// https://github.com/bytecodealliance/wasmtime/blob/b943666650696f1eb7ff8b217762b58d5ef5779d/src/commands/serve.rs#L641-L656
fn use_pooling_allocator_by_default() -> anyhow::Result<Option<bool>> {
    const BITS_TO_TEST: u32 = 42;
    let mut config = wasmtime::Config::new();
    config.wasm_memory64(true);
    config.static_memory_maximum_size(1 << BITS_TO_TEST);
    let engine = wasmtime::Engine::new(&config)?;
    let mut store = wasmtime::Store::new(&engine, ());
    // NB: the maximum size is in wasm pages to take out the 16-bits of wasm
    // page size here from the maximum size.
    let ty = wasmtime::MemoryType::new64(0, Some(1 << (BITS_TO_TEST - 16)));
    if wasmtime::Memory::new(&mut store, ty).is_ok() {
        Ok(Some(true))
    } else {
        Ok(None)
    }
}

#[instrument(level = "trace", skip(adapter))]
async fn instantiate_pre(
    adapter: &[u8],
    workload: &str,
) -> anyhow::Result<(InstancePre<Ctx>, Engine)> {
    let mut opts = wasmtime_cli_flags::CommonOptions::try_parse_from(iter::empty::<&'static str>())
        .context("failed to construct common Wasmtime options")?;
    let mut config = opts
        .config(None, use_pooling_allocator_by_default().unwrap_or(None))
        .context("failed to construct Wasmtime config")?;
    config.wasm_component_model(true);
    config.async_support(true);
    let engine = wasmtime::Engine::new(&config).context("failed to initialize Wasmtime engine")?;

    let wasm = if workload.starts_with('.') || workload.starts_with('/') {
        fs::read(&workload)
            .await
            .with_context(|| format!("failed to read relative path to workload `{workload}`"))
            .map(Workload::Binary)
    } else {
        Url::parse(workload)
            .with_context(|| format!("failed to parse Wasm URL `{workload}`"))
            .map(Workload::Url)
    }?;
    let wasm = match wasm {
        Workload::Url(wasm) => match wasm.scheme() {
            "file" => {
                let wasm = wasm
                    .to_file_path()
                    .map_err(|()| anyhow!("failed to convert Wasm URL to file path"))?;
                fs::read(wasm)
                    .await
                    .context("failed to read Wasm from file URL")?
            }
            "http" | "https" => {
                let wasm = reqwest::get(wasm).await.context("failed to GET Wasm URL")?;
                let wasm = wasm.bytes().await.context("failed fetch Wasm from URL")?;
                wasm.to_vec()
            }
            scheme => bail!("URL scheme `{scheme}` not supported"),
        },
        Workload::Binary(wasm) => wasm,
    };
    let wasm = if wasmparser::Parser::is_core_wasm(&wasm) {
        wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&wasm)
            .context("failed to set core component module")?
            .adapter(WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, adapter)
            .context("failed to add WASI adapter")?
            .encode()
            .context("failed to encode a component")?
    } else {
        wasm
    };

    let component = Component::new(&engine, wasm).context("failed to compile component")?;

    let mut linker = Linker::<Ctx>::new(&engine);
    wasmtime_wasi::add_to_linker_async(&mut linker).context("failed to link WASI")?;
    wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)
        .context("failed to link `wasi:http`")?;

    bindings::wasi::keyvalue::atomics::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:keyvalue/atomics`")?;
    bindings::wasi::keyvalue::batch::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:keyvalue/batch`")?;
    bindings::wasi::keyvalue::store::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:keyvalue/store`")?;

    bindings::wasi::messaging::types::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:messaging/types`")?;
    bindings::wasi::messaging::producer::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:messaging/producer`")?;
    bindings::wasi::messaging::request_reply::add_to_linker(&mut linker, |ctx| ctx)
        .context("failed to link `wasi:messaging/request_reply`")?;

    let pre = linker
        .instantiate_pre(&component)
        .context("failed to pre-instantiate component")?;
    Ok((pre, engine))
}

fn handle_message(
    tasks: &mut JoinSet<anyhow::Result<()>>,
    mut store: Store<Ctx>,
    pre: messaging::bindings::MessagingGuestPre<Ctx>,
    msg: messaging::Message,
) {
    tasks.spawn(async move {
        let component = pre
            .instantiate_async(&mut store)
            .await
            .context("failed to instantiate `command`")?;
        let msg = store
            .data_mut()
            .table
            .push(msg)
            .context("failed to push message to table")?;
        let res = component
            .wasi_messaging_incoming_handler()
            .call_handle(&mut store, msg)
            .await
            .context("failed to invoke component")?;
        res.context("failed to handle NATS.io message")
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact().without_time())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Args { workload } = Args::parse();
    let Config { trigger, nats, .. } = match fs::read("wex.toml").await {
        Ok(buf) => {
            toml::from_str(&String::from_utf8_lossy(&buf)).context("failed to parse `wex.toml`")?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("`wex.toml` not found, using defaults");
            Default::default()
        }
        Err(err) => {
            bail!(anyhow!(err).context("failed to read `wex.toml`"))
        }
    };

    let nats_addr = Arc::from(nats.address);
    let nats_once = OnceCell::new();

    let (pre, engine) = instantiate_pre(WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER, &workload).await?;
    let new_store = || {
        Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new()
                    .inherit_env()
                    .inherit_stdio()
                    .inherit_network()
                    .allow_ip_name_lookup(true)
                    .allow_tcp(true)
                    .allow_udp(true)
                    .args(&["reactor.wasm".to_string()])
                    .build(),
                http: WasiHttpCtx::new(),
                table: ResourceTable::new(),
                nats_addr: Arc::clone(&nats_addr),
                nats_once: nats_once.clone(),
            },
        )
    };

    let messaging_pre = (!trigger.nats.is_empty())
        .then(|| messaging::bindings::MessagingGuestPre::new(pre))
        .transpose()
        .context("failed to pre-instantiate `wasi:messaging`")?;
    let nats_msgs = if !trigger.nats.is_empty() {
        let subs = Box::into_iter(trigger.nats).map(|config::NatsTrigger { subject, group }| {
            let nats_addr = Arc::clone(&nats_addr);
            let nats_once = nats_once.clone();
            async move {
                let nats = nats_client(&nats_once, nats_addr.as_ref()).await?;
                let subject = async_nats::Subject::from(subject.into_string());
                if !group.is_empty() {
                    nats.queue_subscribe(subject, group.into_string()).await
                } else {
                    nats.subscribe(subject).await
                }
                .context("failed to subscribe")
            }
        });
        try_join_all(subs).await?
    } else {
        Vec::default()
    };

    let mut nats_msgs = stream::select_all(nats_msgs);
    let shutdown = signal::ctrl_c();
    let mut shutdown = pin!(shutdown);
    let mut tasks = JoinSet::new();
    loop {
        select! {
            Some(msg) = nats_msgs.next() => {
                handle_message(
                    &mut tasks,
                    new_store(),
                    messaging_pre.clone().unwrap(),
                    messaging::Message::Nats(msg),
                );
            },
            Some(res) = tasks.join_next() => {
                if let Err(err) = res {
                    error!(?err, "failed to join task")
                }
            },
            res = &mut shutdown => {
                // wait for all invocations to complete
                while let Some(res) = tasks.join_next().await {
                    if let Err(err) = res {
                        error!(?err, "failed to join task")
                    }
                }
                return res.context("failed to listen for ^C")
            },
        }
    }
}
