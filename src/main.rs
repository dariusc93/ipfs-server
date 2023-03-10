mod config;
use std::{error::Error, path::PathBuf};

use base64::{
    alphabet::STANDARD,
    engine::{general_purpose::PAD, GeneralPurpose},
    Engine,
};
use clap::Parser;
use rust_ipfs::{
    p2p::{IdentifyConfiguration, KadConfig, KadInserts, PeerInfo, SwarmConfig, TransportConfig},
    IpfsOptions, Keypair, Multiaddr, Protocol, UninitializedIpfs,
};
use tokio::sync::Notify;

use crate::config::IpfsConfig;

#[derive(Debug, Parser)]
#[clap(name = "ipfs-server")]
struct Options {
    /// Setting path to use for persistence storage
    #[clap(short, long)]
    path: Option<PathBuf>,

    /// Path to protobuf keypair
    #[clap(short, long)]
    keypair: Option<PathBuf>,

    /// Path to IPFS configuration to use keypair
    #[clap(short, long)]
    config: Option<PathBuf>,

    /// List of listening addresses in Multiaddr format (eg /ip4/0.0.0.0/tcp/0)
    #[clap(short, long)]
    listen_address: Vec<Multiaddr>,

    /// List of relays to use. Note: This will disable the use of the relay server
    #[clap(short, long)]
    relays: Vec<Multiaddr>,

    /// Disable bootstrapping. Note: Disabling bootstrapping will not announce your node to DHT.
    #[clap(short, long)]
    disable_bootstrap: bool,

    /// Use default ipfs bootstrapping node.
    #[clap(long)]
    default_bootstrap: bool,

    /// List of bootstrap nodes in Multiaddr format (eg /dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN)
    #[clap(short, long)]
    bootstraps: Vec<Multiaddr>,
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
        .set_kad_configuration(
            KadConfig {
                insert_method: KadInserts::Manual,
                ..Default::default()
            },
            None,
        )
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

    if !opt.bootstraps.is_empty() {
        for addr in opt.bootstraps {
            uninitialized = uninitialized.add_bootstrap(addr);
        }
    }

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
            if let Err(e) = ipfs.connect(relay.clone()).await {
                println!("Error dialing relay: {e}");
                continue;
            }

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
        if opt.default_bootstrap {
            ipfs.default_bootstrap().await?;
        }
        if !ipfs.get_bootstraps().await?.is_empty() {
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
