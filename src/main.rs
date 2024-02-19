mod arguments;
mod config;

use std::{error::Error, path::PathBuf};

use arguments::IpfsCommand;
use base64::{
    alphabet::STANDARD,
    engine::{general_purpose::PAD, GeneralPurpose},
    Engine,
};
use clap::Parser;
use rust_ipfs::{FDLimit, Keypair, Multiaddr, Protocol, UninitializedIpfs};

use crate::config::IpfsConfig;

#[derive(Debug, Parser)]
#[clap(name = "ipfs-server")]
struct Options {
    /// Setting path to use for persistence storage
    #[clap(short, long)]
    path: Option<PathBuf>,

    /// Path to protobuf-encoded keypair
    #[clap(short, long)]
    keypair: Option<PathBuf>,

    /// Path to IPFS configuration. Note: This will only be used to read the keypair from the ipfs config file
    #[clap(short, long)]
    config: Option<PathBuf>,

    /// List of listening addresses in Multiaddr format (eg /ip4/0.0.0.0/tcp/0)
    #[clap(short, long)]
    listen_address: Vec<Multiaddr>,

    /// List of relays to use. Note: This will disable the use of the relay server
    #[clap(short, long)]
    relays: Vec<Multiaddr>,

    /// List of bootstrap nodes in Multiaddr format (eg /dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN)
    #[clap(short, long)]
    bootstraps: Vec<Multiaddr>,

    #[command(subcommand)]
    command: Option<IpfsCommand>,
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
            if path.is_file() {
                let keypair_data = zeroize::Zeroizing::new(tokio::fs::read_to_string(path).await?);
                let engine = GeneralPurpose::new(&STANDARD, PAD);
                let bytes = zeroize::Zeroizing::new(engine.decode(keypair_data.as_bytes())?);
                keypair = Keypair::from_protobuf_encoding(&bytes).ok();
            } else {
                let kp = Keypair::generate_ed25519();
                let engine = GeneralPurpose::new(&STANDARD, PAD);
                let data = zeroize::Zeroizing::new(engine.encode(kp.to_protobuf_encoding()?));
                tokio::fs::write(path, &data).await?;
                keypair = Some(kp);
            }
        }
    }

    let mut uninitialized = UninitializedIpfs::new()
        .with_default()
        .with_relay(true)
        .with_upnp()
        .fd_limit(FDLimit::Max)
        .with_custom_behaviour(ext_behaviour::Behaviour::default());

    if let Some(keypair) = keypair {
        uninitialized = uninitialized.set_keypair(&keypair);
    }

    if !opt.listen_address.is_empty() {
        uninitialized = uninitialized.add_listening_addrs(opt.listen_address);
    }

    if opt.relays.is_empty() {
        uninitialized = uninitialized.with_relay_server(Default::default());
    }

    if let Some(path) = opt.path {
        uninitialized = uninitialized.set_path(path);
    }

    let ipfs = uninitialized.start().await?;

    if !opt.relays.is_empty() {
        for relay in opt.relays {
            if let Err(e) = ipfs
                .add_listening_address(relay.with(Protocol::P2pCircuit))
                .await
            {
                println!("Error listening on relay circuit: {e}");
                continue;
            }
        }
    }
    match opt.bootstraps.is_empty() {
        true => {
            ipfs.default_bootstrap().await?;
        }
        false => {
            for addr in opt.bootstraps {
                ipfs.add_bootstrap(addr).await?;
            }
        }
    }

    if !ipfs.get_bootstraps().await?.is_empty() {
        ipfs.bootstrap().await?;
    }

    match opt.command {
        Some(command) => arguments::arguments(&ipfs, command).await?,
        None => tokio::signal::ctrl_c().await?,
    }

    ipfs.exit_daemon().await;
    Ok(())
}

mod ext_behaviour {
    use std::{
        collections::{HashMap, HashSet},
        task::{Context, Poll},
    };

    use rust_ipfs::libp2p::{
        core::Endpoint,
        swarm::{
            derive_prelude::ConnectionEstablished, ConnectionClosed, ConnectionDenied,
            ConnectionId, FromSwarm, NewListenAddr, THandler, THandlerInEvent, THandlerOutEvent,
            ToSwarm,
        },
        Multiaddr, PeerId,
    };
    use rust_ipfs::{libp2p::swarm::derive_prelude::ExternalAddrConfirmed, NetworkBehaviour};

    #[derive(Default, Debug)]
    pub struct Behaviour {
        listener_addrs: HashSet<Multiaddr>,
        connections: HashMap<ConnectionId, PeerId>,
    }

    impl NetworkBehaviour for Behaviour {
        type ConnectionHandler = rust_ipfs::libp2p::swarm::dummy::ConnectionHandler;
        type ToSwarm = void::Void;

        fn handle_pending_inbound_connection(
            &mut self,
            _: ConnectionId,
            _: &Multiaddr,
            _: &Multiaddr,
        ) -> Result<(), ConnectionDenied> {
            Ok(())
        }

        fn handle_pending_outbound_connection(
            &mut self,
            _: ConnectionId,
            _: Option<PeerId>,
            _: &[Multiaddr],
            _: Endpoint,
        ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
            Ok(vec![])
        }

        fn handle_established_inbound_connection(
            &mut self,
            _: ConnectionId,
            _: PeerId,
            _: &Multiaddr,
            _: &Multiaddr,
        ) -> Result<THandler<Self>, ConnectionDenied> {
            Ok(rust_ipfs::libp2p::swarm::dummy::ConnectionHandler)
        }

        fn handle_established_outbound_connection(
            &mut self,
            _: ConnectionId,
            _: PeerId,
            _: &Multiaddr,
            _: Endpoint,
        ) -> Result<THandler<Self>, ConnectionDenied> {
            Ok(rust_ipfs::libp2p::swarm::dummy::ConnectionHandler)
        }

        fn on_connection_handler_event(
            &mut self,
            _: PeerId,
            _: ConnectionId,
            _: THandlerOutEvent<Self>,
        ) {
        }

        fn on_swarm_event(&mut self, event: FromSwarm) {
            match event {
                FromSwarm::NewListenAddr(NewListenAddr { addr, .. }) => {
                    if self.listener_addrs.insert(addr.clone()) {
                        println!("Listening on {addr}");
                    }
                }
                FromSwarm::ExternalAddrConfirmed(ExternalAddrConfirmed { addr }) => {
                    if self.listener_addrs.insert(addr.clone()) {
                        println!("Listening on {addr}");
                    }
                }
                FromSwarm::ConnectionEstablished(ConnectionEstablished {
                    peer_id,
                    connection_id,
                    ..
                }) => {
                    self.connections.insert(connection_id, peer_id);
                    println!("Connections: {}", self.connections.len());
                }
                FromSwarm::ConnectionClosed(ConnectionClosed {
                    peer_id,
                    connection_id,
                    ..
                }) => {
                    let peer_id_opt = self.connections.remove(&connection_id);
                    if let Some(id) = peer_id_opt {
                        assert_eq!(id, peer_id);
                    }
                    println!("Connections: {}", self.connections.len());
                }
                _ => {}
            }
        }

        fn poll(&mut self, _: &mut Context) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
            Poll::Pending
        }
    }
}
