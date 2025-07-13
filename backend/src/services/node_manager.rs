//! Manages connections and interactions with Lightning Network nodes (LND and CLN).
//!
//! This module defines connection structures (`LndConnection`, `ClnConnection`),
//! manages authenticated node instances (`LndNode`, `ClnNode`), handles their lifecycle,
//! and provides methods for interacting with the Lightning node RPCs.

use std::collections::HashSet;
use std::str::FromStr;
use serde::{Serialize, Deserialize};
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use crate::utils;
use crate::utils::{NodeId, NodeInfo};
use tokio::sync::Mutex;
use crate::errors::LightningError;
use lightning::ln::features::NodeFeatures;
use tonic_lnd::{Client, lnrpc::GetInfoRequest};
use cln_grpc::pb::{node_client::NodeClient, GetinfoRequest, ListchannelsRequest};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, Error};

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ConnectionRequest {
    Lnd(LndConnection),
    Cln(ClnConnection),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LndConnection {
    #[serde(with = "utils::serde_node_id")]
    pub id: NodeId,
    #[serde(with = "utils::serde_address")]
    pub address: String,
    #[serde(deserialize_with = "utils::deserialize_path")]
    pub macaroon: String,
    #[serde(deserialize_with = "utils::deserialize_path")]
    pub cert: String,
}

pub struct LndNode {
    pub client: Mutex<Client>,
    pub info: NodeInfo,
    network: Network,
}

/// Parses the node features from the format returned by LND gRPC to LDK NodeFeatures
fn parse_node_features(features: HashSet<u32>) -> NodeFeatures {
    let mut flags = vec![0; 256];

    for f in features.into_iter() {
        let byte_offset = (f / 8) as usize;
        let mask = 1 << (f % 8);
        if flags.len() <= byte_offset {
            flags.resize(byte_offset + 1, 0u8);
        }

        flags[byte_offset] |= mask
    }

    NodeFeatures::from_le_bytes(flags)
}

impl LndNode {
    pub async fn new(connection: LndConnection) -> Result<Self, LightningError> {
        let mut client =
            tonic_lnd::connect(connection.address, connection.cert, connection.macaroon)
                .await
                .map_err(|err| LightningError::ConnectionError(err.to_string()))?;

        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        let mut alias = info.alias;
        let pubkey = PublicKey::from_str(&info.identity_pubkey)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        connection.id.validate(&pubkey, &mut alias)?;

        let network = {
            if info.chains.is_empty() {
                return Err(LightningError::GetInfoError(
                    "node is not connected to any chain".to_string(),
                ));
            } else if info.chains.len() > 1 {
                return Err(LightningError::GetInfoError(format!(
                    "node is connected to more than one chain: {:?}",
                    info.chains.iter().map(|c| c.chain.to_string())
                )));
            }

            Network::from_str(match info.chains[0].network.as_str() {
                "mainnet" => "bitcoin",
                x => x,
            })
            .map_err(|e| LightningError::GetInfoError(e.to_string()))?
        };

        Ok(Self {
            client: Mutex::new(client),
            info: NodeInfo {
                pubkey,
                features: parse_node_features(info.features.keys().cloned().collect()),
                alias,
            },
            network,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClnConnection {
    #[serde(with = "utils::serde_node_id")]
    pub id: NodeId,
    #[serde(with = "utils::serde_address")]
    pub address: String,
    #[serde(deserialize_with = "utils::deserialize_path")]
    pub ca_cert: String,
    #[serde(deserialize_with = "utils::deserialize_path")]
    pub client_cert: String,
    #[serde(deserialize_with = "utils::deserialize_path")]
    pub client_key: String,
}

pub struct ClnNode {
    pub client: Mutex<NodeClient<Channel>>,
    pub info: NodeInfo,
    network: Network,
}

impl ClnNode {
    pub async fn new(connection: ClnConnection) -> Result<Self, LightningError> {
        let tls = ClientTlsConfig::new()
            .domain_name("cln")
            .identity(Identity::from_pem(
                reader(&connection.client_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot load client certificate: {}",
                        err
                    ))
                })?,
                reader(&connection.client_key).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load client key: {}", err))
                })?,
            ))
            .ca_certificate(Certificate::from_pem(
                reader(&connection.ca_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load CA certificate: {}", err))
                })?,
            ));

        let grpc_connection = Channel::from_shared(connection.address)
                .map_err(|err| LightningError::ConnectionError(err.to_string()))?
                .tls_config(tls)
                .map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot establish tls connection: {}",
                        err
                    ))
                })?
                .connect()
                .await
                .map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot connect to gRPC server: {}",
                        err
                ))
        })?;
        let client = Mutex::new(NodeClient::new(grpc_connection));
        let info = client
            .lock()
            .await
            .getinfo(GetinfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

         let pubkey = PublicKey::from_slice(&info.id)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let mut alias = info.alias.unwrap_or_default();
        connection.id.validate(&pubkey, &mut alias)?;

        let features = match info.our_features {
            Some(features) => NodeFeatures::from_be_bytes(features.node),
            None => NodeFeatures::empty(),
        };

        let network = Network::from_core_arg(&info.network)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey,
                features,
                alias,
            },
            network,
        })
    }

    /// Fetch channels belonging to the local node, initiated locally if is_source is true, and initiated remotely if
    /// is_source is false. Introduced as a helper function because CLN doesn't have a single API to list all of our
    /// node's channels.
    async fn node_channels(&self, is_source: bool) -> Result<Vec<u64>, LightningError> {
        let req = if is_source {
            ListchannelsRequest {
                source: Some(self.info.pubkey.serialize().to_vec()),
                ..Default::default()
            }
        } else {
            ListchannelsRequest {
                destination: Some(self.info.pubkey.serialize().to_vec()),
                ..Default::default()
            }
        };

        let resp = self
            .client
            .lock()
            .await
            .list_channels(req)
            .await
            .map_err(|err| LightningError::ListChannelsError(err.to_string()))?
            .into_inner();

        Ok(resp
            .channels
            .into_iter()
            .map(|channel| channel.amount_msat.unwrap_or_default().msat)
            .collect())
    }
}

async fn reader(filename: &str) -> Result<Vec<u8>, Error> {
    let mut file = File::open(filename).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;
    Ok(contents)
}