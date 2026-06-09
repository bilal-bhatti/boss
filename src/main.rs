//! Entry point: connect to MQTT, stand up the Matter node, and bridge
//! discovered Home Assistant switches into it.

use core::pin::pin;
use std::net::UdpSocket;
use std::process::id;
use std::time::{SystemTime, UNIX_EPOCH};

use embassy_futures::select::{select, select4};
use futures_lite::StreamExt;

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::net_comm::SharedNetworks;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::endpoints;
use rs_matter::dm::events::NoEvents;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::networks::SysNetifs;
use rs_matter::dm::subscriptions::Subscriptions;
use rs_matter::dm::{Async, ChainedHandler, Dataver, EpClMatcher};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::pairing::qr::QrTextType;
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::{DirKvBlobStore, SharedKvBlobStore};
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::utils::select::Coalesce;
use rs_matter::utils::storage::pooled::PooledBuffers;
use rs_matter::{Matter, MATTER_PORT};

use boss::bridge::node::AGGREGATOR_ENDPOINT;
use boss::bridge::{Bridge, RootEndpoints};
use boss::config::Config;
use boss::discovery::{DiscoveryTopic, SwitchConfig};
use boss::mqtt::{self, Incoming};

mod mdns;

fn main() {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    if let Err(e) = run() {
        log::error!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    let cfg = Config::from_args().map_err(|e| {
        log::error!("{e}");
        ErrorCode::InvalidData
    })?;

    // --- MQTT ---
    // Subscriptions are (re)established on every connection via the `connected`
    // signal below, so there's no one-shot subscribe here — the first ConnAck
    // drives the initial subscribe through the same path as every reconnect.
    let mqtt::MqttConn {
        client,
        incoming,
        connected,
    } = mqtt::connect(&cfg, &client_id()).map_err(|e| {
        log::error!("{e}");
        ErrorCode::StdIoError
    })?;
    let discovery = cfg.discovery_filter("switch");

    // --- Matter plumbing (mirrors the rs-matter bridge example) ---
    let mut matter = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);

    let mut kv_buf = [0u8; 4096];
    let mut kv = DirKvBlobStore::new(cfg.state_dir.clone());
    log::info!("matter state dir: {}", cfg.state_dir.display());
    futures_lite::future::block_on(matter.load_persist(&mut kv, &mut kv_buf))?;

    let buffers = PooledBuffers::<10, _>::new(0);
    let subscriptions: Subscriptions = Subscriptions::new();
    let events = NoEvents::new();

    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);
    let mut rand = crypto.rand()?;

    // --- The bridge + handler tree ---
    let bridge = Bridge::new(client, &mut rand);

    // Root + aggregator handlers; everything else falls through to the bridge.
    let root = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(rand)
        .chain(
            EpClMatcher::new(
                Some(AGGREGATOR_ENDPOINT),
                Some(desc::DescHandler::CLUSTER.id),
            ),
            Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut rand)).adapt()),
        );

    let handler = ChainedHandler::new(RootEndpoints, root, &bridge);
    let dm_handler = (&bridge, handler);

    let dm = rs_matter::dm::DataModel::new(
        &matter,
        &crypto,
        &buffers,
        &subscriptions,
        &events,
        dm_handler,
        SharedKvBlobStore::new(kv, kv_buf.as_mut_slice()),
        SharedNetworks::new(EthNetwork::new_default()),
    );

    let responder = DefaultResponder::new(&dm);

    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)?;

    if !matter.is_commissioned() {
        matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;
    }

    // Commissioning QR + manual code for the status page (constant; computed once).
    let comm_info = build_commissioning()?;

    // --- Run everything ---
    let mut transport = pin!(matter.run(&crypto, &socket, &socket, &socket));
    // mDNS is a hard requirement: without it the Matter node is undiscoverable
    // and effectively broken. So a failure here is fatal — we log it and let the
    // error propagate, which exits the process so systemd restarts us (and waits
    // for avahi via the unit's `Wants`/`After`).
    let mut mdns = pin!(async {
        mdns::run_mdns(&matter, &crypto).await.inspect_err(|e| {
            log::error!("mDNS responder failed ({e}); avahi/mDNS is required — exiting to retry");
        })
    });
    let mut respond = pin!(responder.run::<4, 4>());
    let mut dm_job = pin!(dm.run());
    let mut router = pin!(run_router(&bridge, &cfg, incoming));
    let mut sigint = pin!(wait_for_sigint());
    let mut web = pin!(boss::web::run(&bridge, cfg.http_port, &comm_info));
    let mut resub = pin!(run_resubscribe(&bridge, connected, &discovery));

    let core = select4(&mut transport, &mut mdns, &mut respond, &mut dm_job).coalesce();
    let aux = select4(&mut router, &mut sigint, &mut web, &mut resub).coalesce();

    futures_lite::future::block_on(select(core, aux).coalesce())
}

/// Re-subscribe everything on each broker (re)connection. The first signal does
/// the initial subscribe; every later one recovers from a broker restart or
/// network blip (a clean MQTT session starts with no subscriptions).
async fn run_resubscribe(
    bridge: &Bridge,
    connected: async_channel::Receiver<()>,
    discovery: &str,
) -> Result<(), Error> {
    while connected.recv().await.is_ok() {
        bridge.resubscribe(discovery);
    }
    log::info!("mqtt connection closed; resubscribe loop ended");
    Ok(())
}

/// Compute the Matter onboarding QR (as SVG) + manual pairing code shown on the
/// status page. These derive only from the fixed device commissioning data, so
/// they're constant for the life of the process.
fn build_commissioning() -> Result<boss::web::Commissioning, Error> {
    use rs_matter::pairing::qr::{no_optional_data, CommFlowType, Qr, QrPayload};

    let payload = QrPayload::new_from_basic_info(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        TEST_DEV_COMM.clone(),
        &TEST_DEV_DET,
        no_optional_data,
    );

    let mut text_buf = [0u8; 256];
    let (text, _) = payload.as_str(&mut text_buf)?;
    let qr_text = text.to_string();

    let mut tmp_buf = [0u8; 1024];
    let mut out_buf = [0u8; 1024];
    let qr = Qr::compute(text, &mut tmp_buf, &mut out_buf)?;
    let qr_svg = boss::web::qr_svg(&qr);

    let manual_code = TEST_DEV_COMM.compute_pretty_pairing_code().to_string();

    Ok(boss::web::Commissioning {
        qr_svg,
        manual_code,
        qr_text,
    })
}

/// Consume incoming MQTT messages: discovery → bridge a device; everything else
/// → route state/availability updates to the right slot.
async fn run_router(
    bridge: &Bridge,
    cfg: &Config,
    incoming: async_channel::Receiver<Incoming>,
) -> Result<(), Error> {
    while let Ok(msg) = incoming.recv().await {
        match DiscoveryTopic::parse(&cfg.discovery_prefix, &msg.topic) {
            Ok(topic) if topic.component == "switch" => {
                match SwitchConfig::parse(&msg.topic, &msg.payload) {
                    Ok(sw) => {
                        if let Err(e) = bridge.add_switch(&sw) {
                            log::warn!("{e}");
                        }
                    }
                    Err(e) => log::warn!("{e}"),
                }
            }
            _ => bridge.deliver(&msg.topic, &msg.payload).await,
        }
    }
    log::info!("mqtt stream ended");
    Ok(())
}

/// Resolve when the process receives SIGINT, so `block_on` returns cleanly.
async fn wait_for_sigint() -> Result<(), Error> {
    use async_signal::{Signal, Signals};

    let mut signals = Signals::new([Signal::Int]).map_err(|_| ErrorCode::StdIoError)?;
    signals.next().await;
    log::info!("received SIGINT, shutting down");
    Ok(())
}

/// A unique-enough MQTT client id.
fn client_id() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("boss-{}-{:x}", id(), nonce)
}
