mod config;
use std::{error::Error, path::PathBuf};

use base64::{
    alphabet::STANDARD,
    engine::{general_purpose::PAD, GeneralPurpose},
    Engine,
};
use clap::Parser;
use rust_ipfs::{
    p2p::{IdentifyConfiguration, PeerInfo, SwarmConfig, TransportConfig},
    IpfsOptions, Keypair, Multiaddr, Protocol, UninitializedIpfs,
};
use tokio::sync::Notify;

use crate::config::IpfsConfig;

#[derive(Debug, Parser)]
#[clap(name = "ipfs-server")]
struct Options {
    #[clap(short, long)]
    path: Option<PathBuf>,

    #[clap(short, long)]
    keypair: Option<PathBuf>,

    #[clap(short, long)]
    config: Option<PathBuf>,

    #[clap(short, long)]
    listen_address: Vec<Multiaddr>,

    #[clap(short, long)]
    relays: Vec<Multiaddr>,

    #[clap(short, long)]
    disable_bootstrap: bool,
}

#[tokio::main]
#[allow(unreachable_code)]
async fn main() -> Result<(), Box<dyn Error>> {
    let opt = Options::parse();

    let mut keypair: Option<Keypair> = None;

    if let Some(path) = opt.config {
        let config = IpfsConfig::load(path)?;
        keypair = config.identity.keypair().ok()
    }

    if keypair.is_none() {
        if let Some(path) = opt.keypair {
            let keypair_data = zeroize::Zeroizing::new(tokio::fs::read_to_string(path).await?);
            let engine = GeneralPurpose::new(&STANDARD, PAD);
            let bytes = zeroize::Zeroizing::new(engine.decode(keypair_data.as_bytes())?);
            keypair = Keypair::from_protobuf_encoding(&bytes).ok();
        }
    }

    let mut options = IpfsOptions::default();

    if let Some(keypair) = keypair {
        options.keypair = keypair;
    }

    let mut uninitialized = UninitializedIpfs::with_opt(options)
        .enable_mdns()
        .enable_relay(true)
        .enable_upnp()
        .disable_delay()
        .set_swarm_configuration(SwarmConfig {
            notify_handler_buffer_size: 32.try_into()?,
            connection_event_buffer_size: 1024.try_into()?,
            ..Default::default()
        })
        .set_transport_configuration(TransportConfig {
            yamux_max_buffer_size: 16 * 1024 * 1024,
            yamux_receive_window_size: 16 * 1024 * 1024,
            mplex_max_buffer_size: usize::MAX / 2,
            ..Default::default()
        })
        .set_identify_configuration(IdentifyConfiguration {
            agent_version: "ipfs-server/0.1.0".into(),
            push_update: true,
            cache: 200,
            ..Default::default()
        });

    if !opt.listen_address.is_empty() {
        for addr in opt.listen_address {
            uninitialized = uninitialized.add_listening_addr(addr);
        }
    }

    if opt.relays.is_empty() {
        uninitialized = uninitialized.enable_relay_server(None);
    }

    if let Some(path) = opt.path {
        uninitialized = uninitialized.set_path(path);
    }

    let ipfs = uninitialized.start().await?;

    if !opt.relays.is_empty() {
        for relay in opt.relays {
            if let Err(e) = ipfs.dial(relay.clone()).await {
                println!("Error dialing relay: {e}");
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = ipfs
                .add_listening_address(relay.with(Protocol::P2pCircuit))
                .await
            {
                println!("Error listening on relay circuit: {e}");
                continue;
            }
        }
    }

    if !opt.disable_bootstrap {
        ipfs.default_bootstrap().await?;
        tokio::spawn({
            let ipfs = ipfs.clone();
            async move {
                loop {
                    ipfs.bootstrap().await?;
                    tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
                }
                Ok::<_, Box<dyn Error + Send>>(())
            }
        });
    }

    // Used to give time after bootstrapping to populate the addresses
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let PeerInfo {
        public_key: key,
        listen_addrs: addresses,
        ..
    } = ipfs.identity(None).await?;

    println!("PeerID: {}", key.to_peer_id());

    for address in addresses {
        println!("Listening on: {address}");
    }

    // Used to wait until the process is terminated instead of creating a loop
    Notify::new().notified().await;

    ipfs.exit_daemon().await;
    Ok(())
}
