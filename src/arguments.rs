use std::{error::Error, path::PathBuf};

use base64::{
    alphabet::STANDARD,
    engine::{general_purpose::PAD, GeneralPurpose},
    Engine,
};
use clap::{Args, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use libipld::{Cid, prelude::Codec, Ipld, serde::to_ipld};
use rust_ipfs::{
    libp2p::futures::StreamExt,
    unixfs::{NodeItem, UnixfsStatus},
    Ipfs, IpfsPath, PeerId, PinMode,
};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Subcommand)]
pub enum IpfsCommand {
    /// Show info about IPFS peers
    Id { peer_id: Option<PeerId> },

    /// Add a file to IPFS
    Add { path: PathBuf },

    /// List directory contents for unixfs objects
    Ls { path: IpfsPath },

    /// Pin objects to local storage
    Pin(PinArg),

    /// Show IPFS object data
    Cat { path: IpfsPath },

    /// Download IPFS objects
    Get {
        path: IpfsPath,
        local: Option<PathBuf>,
    },

    /// Interact with IPLD DAG objects
    Dag(DagArg),

    /// Query the DHT for values or peers
    Dht(DhtArg),

    /// Manipulate the IPFS repository
    Repo(RepoArg),
}

#[derive(Debug, Args)]
pub struct RepoArg {
    #[command(subcommand)]
    command: RepoCommand,
}

#[derive(Debug, Args)]
pub struct DhtArg {
    #[command(subcommand)]
    command: DhtCommand,
}

#[derive(Debug, Args)]
pub struct DagArg {
    #[command(subcommand)]
    command: DagCommand,
}

#[derive(Debug, Args)]
pub struct PinArg {
    #[clap(long, default_value_t = true)]
    recursive: bool,

    #[command(subcommand)]
    command: PinCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Perform a (very basic) garbage collection sweep on the repo
    Gc,
}

#[derive(Debug, Subcommand)]
pub enum PinCommand {
    /// Pin objects to local storage.
    Add { path: Cid },

    /// List objects pinned to local storage.
    Ls,

    /// Remove object from pin-list.
    Rm { path: Cid },
}

#[derive(Debug, Subcommand)]
pub enum DagCommand {
    /// Get a DAG node from IPFS
    Get { path: IpfsPath },

    /// Add a DAG node to IPFS
    Put { object: String },
}

#[derive(Debug, Subcommand)]
pub enum DhtCommand {
    /// Find the multiaddresses associated with a Peer ID.
    Findpeer { peer_id: PeerId },

    /// Find peers that can provide a specific value, given a key.
    Findprovs { key: Cid },
}

pub async fn arguments(ipfs: &Ipfs, command: IpfsCommand) -> Result<(), Box<dyn Error>> {
    let engine = GeneralPurpose::new(&STANDARD, PAD);
    match command {
        IpfsCommand::Id { peer_id } => {
            let info = ipfs.identity(peer_id).await?;
            let public_key = engine.encode(info.public_key.to_protobuf_encoding());
            println!("PeerID: {}", info.peer_id);
            println!("Public Key: {public_key}");
            println!("Addresses:");
            for addr in info.listen_addrs.iter() {
                println!("- {addr}");
            }
            println!("Agent Version: {}", info.agent_version);
            println!("Protocol Version: {}", info.protocol_version);
            println!("Protocols:");
            for protocol in info.protocols.iter() {
                println!("- {protocol}");
            }
        }
        IpfsCommand::Add { path } => {
            let size = tokio::fs::metadata(path.clone()).await?.len();
            let pb = ProgressBar::new(size);
            pb.set_style(ProgressStyle::with_template("{msg}\n {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")?.progress_chars("#>-"));

            let mut status = ipfs.add_file_unixfs(path).await?;

            while let Some(status) = status.next().await {
                match status {
                    UnixfsStatus::ProgressStatus { written, .. } => {
                        pb.set_position(written as _);
                    }
                    UnixfsStatus::CompletedStatus { path, .. } => {
                        pb.finish_with_message(format!("added {path}"));
                    }
                    UnixfsStatus::FailedStatus { error, .. } => {
                        pb.finish_with_message(format!("Error adding file to ipfs: {error:?}"));
                    }
                }
            }
        }
        IpfsCommand::Ls { path } => {
            let mut list = ipfs.ls_unixfs(path).await?;

            while let Some(item) = list.next().await {
                match item {
                    NodeItem::Directory { .. } | NodeItem::RootDirectory { .. } => {}
                    NodeItem::File { cid, file, size } => println!("{} {} {}", cid, size, file),
                    NodeItem::Error { error } => {
                        println!("Error listening item: {error}");
                        break;
                    }
                }
            }
        }
        IpfsCommand::Cat { path } => {
            let mut stream = ipfs.cat_unixfs(path, None).await?.boxed();
            let mut stdout = tokio::io::stdout();

            while let Some(result) = stream.next().await {
                match result {
                    Ok(bytes) => {
                        stdout.write_all(&bytes).await?;
                    }
                    Err(e) => {
                        eprintln!("Error: {e}");
                        break;
                    }
                }
            }
        }
        IpfsCommand::Get { path, local } => {
            let file = match local {
                Some(path) => path,
                None => match path.clone().iter().last() {
                    Some(item) => PathBuf::from(item),
                    None => match path.root().cid() {
                        Some(cid) => PathBuf::from(cid.to_string()),
                        None => panic!("Invalid path"),
                    },
                },
            };

            let pb = ProgressBar::new(0);
            pb.set_style(ProgressStyle::with_template("{msg}\n {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")?.progress_chars("#>-"));

            let mut status = ipfs.get_unixfs(path, file.clone()).await?;

            let mut size_set = false;

            while let Some(status) = status.next().await {
                match status {
                    UnixfsStatus::ProgressStatus {
                        written,
                        total_size,
                    } => {
                        if !size_set {
                            if let Some(size) = total_size {
                                size_set = true;
                                pb.set_length(size as _);
                            }
                        }
                        pb.set_position(written as _);
                    }
                    UnixfsStatus::CompletedStatus { .. } => {
                        pb.finish_with_message(format!("Saved file to {}", file.display()));
                    }
                    UnixfsStatus::FailedStatus { error, .. } => {
                        pb.finish_with_message(format!("Failed to saved file: {error:?}"));
                    }
                }
            }
        }

        IpfsCommand::Dag(DagArg { command }) => match command {
            DagCommand::Get { path } => {
                let object = ipfs.get_dag(path).await?;
                let value = libipld::json::DagJsonCodec.encode(&object)?;
                println!("{}", String::from_utf8_lossy(&value));
            }
            DagCommand::Put { object } => {
                // let bytes = object.as_bytes();
                let object: Ipld = to_ipld(object)?;
                let cid = ipfs.put_dag(object).await?;
                println!("{}", cid);
            }
        },

        IpfsCommand::Dht(DhtArg { command }) => match command {
            DhtCommand::Findpeer { peer_id } => {
                let addresses = ipfs.find_peer(peer_id).await?;
                for addr in addresses {
                    println!("{addr}");
                }
            }
            DhtCommand::Findprovs { key } => {
                let mut provs = ipfs.get_providers(key).await?;
                while let Some(provider) = provs.next().await {
                    println!("{provider}");
                }
            }
        },
        IpfsCommand::Repo(RepoArg { command }) => match command {
            RepoCommand::Gc => {
                let blocks = ipfs.gc().await?;
                for cid in blocks {
                    println!("removed {cid}");
                }
            }
        },
        IpfsCommand::Pin(PinArg { recursive, command }) => match command {
            PinCommand::Add { path } => {
                ipfs.insert_pin(&path, recursive).await?;
                print!("pinned {path} ");
                if recursive {
                    println!("recursively");
                }
            }
            PinCommand::Ls => {
                let mut list = ipfs
                    .list_pins(None)
                    .await
                    .filter_map(|res| async { res.ok() })
                    .map(|(cid, mode)| {
                        (
                            cid,
                            match mode {
                                PinMode::Direct => "direct",
                                PinMode::Indirect => "indirect",
                                PinMode::Recursive => "recursive",
                            },
                        )
                    })
                    .boxed();

                while let Some((cid, mode)) = list.next().await {
                    println!("{cid} {mode}");
                }
            }
            PinCommand::Rm { path } => {
                ipfs.remove_pin(&path, recursive).await?;
                println!("unpinned {path}");
            }
        },
    }

    Ok(())
}
