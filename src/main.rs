use std::error::Error;

use clap::Parser;
use rust_ipfs::{
    p2p::{IdentifyConfiguration, PeerInfo},
    UninitializedIpfs,
};
use tokio::sync::Notify;

#[derive(Debug, Parser)]
#[clap(name = "ipfs-server")]
struct Options {}

#[tokio::main]
#[allow(unreachable_code)]
async fn main() -> Result<(), Box<dyn Error>> {
    let ipfs = UninitializedIpfs::new()
        .enable_mdns()
        .enable_relay(true)
        .enable_relay_server(None)
        .enable_upnp()
        .disable_delay()
        .set_identify_configuration(IdentifyConfiguration {
            agent_version: "ipfs-server/0.1.0".into(),
            push_update: true,
            cache: 200,
            ..Default::default()
        })
        .start()
        .await?;

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
