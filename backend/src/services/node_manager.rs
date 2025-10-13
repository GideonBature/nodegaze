//! Manages connections and interactions with Lightning Network nodes (LND and CLN).
//!
//! This module defines connection structures (`LndConnection`, `ClnConnection`),
//! manages authenticated node instances (`LndNode`, `ClnNode`), handles their lifecycle,
//! and provides methods for interacting with the Lightning node RPCs.

use crate::{
    errors::LightningError,
    services::event_manager::{CLNEvent, LNDEvent, NodeSpecificEvent},
    utils::{
        self, ChannelDetails, ChannelState, ChannelSummary, CustomInvoice, Feature, Hop,
        InvoiceHtlc, InvoiceStatus, NodeId, NodeInfo, NodePolicy, PaymentDetails, PaymentHtlc,
        PaymentState, PaymentSummary, PaymentType, Route, ShortChannelID,
        sats_to_usd::PriceConverter,
    },
};

use async_stream::stream;
use async_trait::async_trait;
use bitcoin::{Network, OutPoint, Txid, secp256k1::PublicKey};
use cln_grpc::pb::{
    GetinfoRequest, ListchannelsRequest, ListpeerchannelsRequest,
    node_client::NodeClient,
};
use futures::stream::{SelectAll, StreamExt};
use hex;
use lightning::ln::{PaymentHash, features::NodeFeatures};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    pin::Pin,
    str::FromStr,
};
use tokio::time::Duration;
use tokio::{
    fs::File,
    io::{AsyncReadExt, Error},
    sync::Mutex,
    time::sleep,
};
use tokio_stream::Stream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use tonic_lnd::{
    Client,
    lnrpc::{
        ChannelEventSubscription, ChannelEventUpdate, ChannelGraphRequest, GetInfoRequest, Invoice,
        InvoiceSubscription, ListChannelsRequest, ListInvoiceRequest, ListPaymentsRequest,
        channel_event_update::{Channel as EventChannel, UpdateType as LndChannelUpdateType},
        invoice::InvoiceState,
        payment::PaymentStatus,
    },
    tonic::Streaming,
};

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
    price_converter: PriceConverter,
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

        Ok(Self {
            client: Mutex::new(client),
            info: NodeInfo {
                pubkey,
                features: parse_node_features(info.features.keys().cloned().collect()),
                alias,
            },
            price_converter: PriceConverter::new(),
        })
    }

    async fn stream_channel_events(&self) -> Result<Streaming<ChannelEventUpdate>, LightningError> {
        println!("Attempting to subscribe to LND channel events...");
        let channel_event_stream: Streaming<ChannelEventUpdate> = match self
            .client
            .lock()
            .await
            .lightning()
            .subscribe_channel_events(ChannelEventSubscription {})
            .await
        {
            Ok(response) => {
                println!("LND channel events subscription successful: {response:?}");
                response.into_inner()
            }
            Err(e) => {
                eprintln!("Error subscribing to LND channel events: {e:?}");
                return Err(LightningError::StreamingError(format!("{e}")));
            }
        };
        println!("Finished channel events subscription block.");
        Ok(channel_event_stream)
    }

    async fn stream_invoice_events(&self) -> Result<Streaming<Invoice>, LightningError> {
        println!("Attempting to subscribe to LND invoice events...");
        let invoice_event_stream = match self
            .client
            .lock()
            .await
            .lightning()
            .subscribe_invoices(InvoiceSubscription {
                add_index: 0,
                settle_index: 0,
            })
            .await
        {
            Ok(response) => response.into_inner(),
            Err(e) => {
                eprintln!("Error subscribing to LND invoice events: {e:?}");
                return Err(LightningError::StreamingError(format!("{e}")));
            }
        };
        println!("Finished invoice events subscription block.");
        Ok(invoice_event_stream)
    }

    async fn get_lightning_stub(&self) -> tonic_lnd::LightningClient {
        let mut client = self.client.lock().await;
        client.lightning().clone()
    }

    async fn process_outgoing_payment(
        &self,
        payment: tonic_lnd::lnrpc::Payment,
    ) -> Result<PaymentDetails, LightningError> {
        let state = match PaymentStatus::try_from(payment.status).unwrap_or(PaymentStatus::Unknown)
        {
            PaymentStatus::Unknown | PaymentStatus::InFlight => PaymentState::Inflight,
            PaymentStatus::Succeeded => PaymentState::Settled,
            PaymentStatus::Failed => PaymentState::Failed,
        };

        let creation_time = payment
            .creation_time_ns
            .try_into()
            .ok()
            .map(|timestamp_ns: u64| timestamp_ns / 1_000_000_000);

        let completed_at = match state {
            PaymentState::Settled => payment.htlcs.last().and_then(|htlc| {
                let resolve_time = htlc.resolve_time_ns as u64;
                if resolve_time > 0 {
                    Some(resolve_time / 1_000_000_000)
                } else {
                    None
                }
            }),
            _ => None,
        };

        // Process HTLCs and extract destination pubkey from the last hop
        let (htlcs, destination_pubkey) = {
            let mut destination_pubkey = None;
            let htlcs = payment
                .htlcs
                .into_iter()
                .map(|htlc| {
                    let route = htlc.route.map(|raw_route| {
                        // Get destination pubkey from last hop if available
                        if let Some(last_hop) = raw_route.hops.last() {
                            if let Ok(pubkey) = PublicKey::from_str(&last_hop.pub_key) {
                                destination_pubkey = Some(pubkey);
                            }
                        }

                        Route {
                            total_time_lock: raw_route.total_time_lock,
                            total_fees: (raw_route.total_fees_msat / 1000).try_into().unwrap_or(0),
                            total_amt: (raw_route.total_amt_msat / 1000).try_into().unwrap_or(0),
                            hops: raw_route
                                .hops
                                .into_iter()
                                .map(|hop| Hop {
                                    pubkey: PublicKey::from_str(&hop.pub_key)
                                        .unwrap_or(self.info.pubkey),
                                    chan_id: ShortChannelID(hop.chan_id),
                                    amount_to_forward: (hop.amt_to_forward_msat / 1000) as u64,
                                    fee: Some((hop.fee_msat / 1000) as u64),
                                    expiry: Some(hop.expiry.into()),
                                })
                                .collect(),
                        }
                    });

                    PaymentHtlc {
                        routes: route.map_or_else(Vec::new, |route| vec![route]),
                        attempt_id: htlc.attempt_id,
                        attempt_time: {
                            let attempt_ns = htlc.attempt_time_ns as u64;
                            (attempt_ns > 0).then_some(attempt_ns / 1_000_000_000)
                        },
                        resolve_time: {
                            let resolve_ns = htlc.resolve_time_ns as u64;
                            (resolve_ns > 0).then_some(resolve_ns / 1_000_000_000)
                        },
                        failure_reason: htlc
                            .failure
                            .as_ref()
                            .map(|failure| format!("{:?}", failure.code())),
                        failure_code: htlc.failure.as_ref().map(|failure| failure.code() as u16),
                    }
                })
                .collect();

            (htlcs, destination_pubkey)
        };

        // Parse invoice for description
        let description = Bolt11Invoice::from_str(&payment.payment_request)
            .ok()
            .and_then(|invoice| {
                if let Bolt11InvoiceDescription::Direct(desc) = invoice.description() {
                    Some(desc.to_string())
                } else {
                    None
                }
            });

        let network = self
            .get_network()
            .await
            .map(|network| Some(network.to_string()))
            .unwrap_or(None);

        let amount_sat: u64 = payment.value_sat.try_into().unwrap_or(0);
        let amount_usd = self.price_converter.sats_to_usd(amount_sat).await?;

        Ok(PaymentDetails {
            state,
            payment_type: PaymentType::Outgoing,
            amount_sat,
            amount_usd,
            routing_fee: Some(payment.fee_sat.try_into().unwrap_or(0)),
            network,
            description,
            creation_time,
            invoice: payment.payment_request.into(),
            payment_hash: payment.payment_hash,
            destination_pubkey,
            completed_at,
            htlcs,
        })
    }

    async fn process_incoming_payment(
        &self,
        invoice: tonic_lnd::lnrpc::Invoice,
    ) -> Result<PaymentDetails, LightningError> {
        let state = match invoice.state {
            0 => {
                // OPEN - check if payment is in progress
                PaymentState::Inflight
            }
            1 => PaymentState::Settled,  // SETTLED
            2 => PaymentState::Failed,   // CANCELED
            3 => PaymentState::Inflight, // ACCEPTED (payment in progress)
            _ => PaymentState::Inflight, // Default to inflight for unknown states
        };

        let creation_time = Some(invoice.creation_date as u64);

        let completed_at = match state {
            PaymentState::Settled | PaymentState::Failed => {
                if invoice.settle_date > 0 {
                    Some(invoice.settle_date as u64)
                } else {
                    None
                }
            }
            _ => None,
        };

        // Process HTLCs for incoming payments
        let htlcs: Vec<PaymentHtlc> = invoice
            .htlcs
            .into_iter()
            .map(|htlc| PaymentHtlc {
                routes: Vec::new(),
                attempt_id: htlc.htlc_index,
                attempt_time: {
                    let accept_ns = htlc.accept_time as u64;
                    (accept_ns > 0).then_some(accept_ns / 1_000_000_000)
                },
                resolve_time: {
                    let resolve_ns = htlc.resolve_time as u64;
                    (resolve_ns > 0).then_some(resolve_ns / 1_000_000_000)
                },
                failure_reason: None,
                failure_code: None,
            })
            .collect();

        // Parse invoice for description
        let description = if !invoice.memo.is_empty() {
            Some(invoice.memo.clone())
        } else {
            Bolt11Invoice::from_str(&invoice.payment_request)
                .ok()
                .and_then(|parsed_invoice| {
                    if let Bolt11InvoiceDescription::Direct(desc) = parsed_invoice.description() {
                        Some(desc.to_string())
                    } else {
                        None
                    }
                })
        };

        let network = self
            .get_network()
            .await
            .map(|network| Some(network.to_string()))
            .unwrap_or(None);

        let amount_sat = if invoice.amt_paid_sat > 0 {
            invoice.amt_paid_sat as u64
        } else {
            invoice.value as u64
        };

        let amount_usd = self.price_converter.sats_to_usd(amount_sat).await?;

        let destination_pubkey = Some(self.info.pubkey);

        Ok(PaymentDetails {
            state,
            payment_type: PaymentType::Incoming,
            amount_sat,
            amount_usd,
            routing_fee: None,
            network,
            description,
            creation_time,
            invoice: Some(invoice.payment_request),
            payment_hash: hex::encode(&invoice.r_hash),
            destination_pubkey,
            completed_at,
            htlcs,
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
    price_converter: PriceConverter,
}

impl ClnNode {
    pub async fn new(connection: ClnConnection) -> Result<Self, LightningError> {
        let tls = ClientTlsConfig::new()
            .domain_name("cln")
            .identity(Identity::from_pem(
                reader(&connection.client_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot load client certificate: {err}"
                    ))
                })?,
                reader(&connection.client_key).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load client key: {err}"))
                })?,
            ))
            .ca_certificate(Certificate::from_pem(
                reader(&connection.ca_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load CA certificate: {err}"))
                })?,
            ));

        let grpc_connection = Channel::from_shared(connection.address)
            .map_err(|err| LightningError::ConnectionError(err.to_string()))?
            .tls_config(tls)
            .map_err(|err| {
                LightningError::ConnectionError(format!("Cannot establish tls connection: {err}"))
            })?
            .connect()
            .await
            .map_err(|err| {
                LightningError::ConnectionError(format!("Cannot connect to gRPC server: {err}"))
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

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey,
                features,
                alias,
            },
            price_converter: PriceConverter::new(),
        })
    }

    async fn get_client_stub(&self) -> NodeClient<Channel> {
        self.client.lock().await.clone()
    }

    async fn get_htlcs_for_payment(
        &self,
        payment_hash: &str,
    ) -> Result<Vec<PaymentHtlc>, LightningError> {
        let mut client = self.get_client_stub().await;

        // Get individual payment attempts (HTLCs) for this payment
        let sendpays_response = client
            .list_send_pays(cln_grpc::pb::ListsendpaysRequest {
                payment_hash: Some(
                    hex::decode(payment_hash)
                        .map_err(|_| LightningError::Parse("Invalid payment hash".to_string()))?,
                ),
                ..Default::default()
            })
            .await
            .map_err(|err| {
                LightningError::PaymentError(format!("CLN list_send_pays error: {err}"))
            })?;

        let htlcs: Vec<PaymentHtlc> = sendpays_response
            .into_inner()
            .payments
            .into_iter()
            .map(|sendpay| PaymentHtlc {
                routes: vec![],
                attempt_id: sendpay.id,
                attempt_time: Some(sendpay.created_at),
                resolve_time: sendpay.completed_at,
                failure_reason: sendpay.erroronion.map(|_| "Payment failed".to_string()),
                failure_code: None,
            })
            .collect();

        Ok(htlcs)
    }

    async fn process_outgoing_payment(
        &self,
        payment: cln_grpc::pb::ListpaysPays,
    ) -> Result<PaymentDetails, LightningError> {
        let state = match payment.status {
            0 => PaymentState::Inflight, // pending
            1 => PaymentState::Settled,  // complete
            2 => PaymentState::Failed,   // failed
            _ => PaymentState::Failed,
        };

        // Calculate amounts
        let amount = payment
            .amount_msat
            .as_ref()
            .map(|amt| amt.msat / 1000)
            .unwrap_or(0);
        let sent_amount = payment
            .amount_sent_msat
            .map(|amt| amt.msat / 1000)
            .unwrap_or(0);
        let routing_fee = sent_amount.checked_sub(amount);

        // Get destination pubkey
        let destination_pubkey = match &payment.destination {
            Some(hex_str) => {
                let hex_str = String::from_utf8(hex_str.clone()).map_err(|err| {
                    LightningError::Parse(format!("Invalid destination string: {err}"))
                })?;
                let pubkey = PublicKey::from_str(&hex_str).map_err(|err| {
                    LightningError::Parse(format!("Invalid destination pubkey: {err}"))
                })?;
                Some(pubkey)
            }
            None => None,
        };

        let creation_time = (payment.created_at > 0).then_some(payment.created_at);

        let network = self
            .get_network()
            .await
            .map(|network| Some(network.to_string()))
            .unwrap_or(None);

        let amount_sat: u64 = payment
            .amount_msat
            .as_ref()
            .map(|amt| amt.msat / 1000)
            .unwrap_or(0);

        let amount_usd = self.price_converter.sats_to_usd(amount_sat).await?;

        // Get HTLC details for this payment
        let payment_hash_hex = hex::encode(&payment.payment_hash);
        let htlcs = self
            .get_htlcs_for_payment(&payment_hash_hex)
            .await
            .unwrap_or_else(|_| vec![]);

        Ok(PaymentDetails {
            state,
            payment_type: PaymentType::Outgoing,
            amount_sat,
            amount_usd,
            routing_fee,
            network,
            description: payment.description,
            creation_time,
            invoice: payment.bolt11,
            payment_hash: payment_hash_hex,
            destination_pubkey,
            completed_at: payment.completed_at,
            htlcs,
        })
    }

    async fn process_incoming_payment(
        &self,
        invoice: cln_grpc::pb::ListinvoicesInvoices,
    ) -> Result<PaymentDetails, LightningError> {
        let state = match invoice.status {
            0 => PaymentState::Inflight, // unpaid
            1 => PaymentState::Settled,  // paid
            2 => PaymentState::Failed,   // expired
            _ => PaymentState::Inflight,
        };

        let creation_time = (invoice.expires_at > 0).then_some(invoice.expires_at);

        let completed_at = match state {
            PaymentState::Settled | PaymentState::Failed => {
                invoice.paid_at.filter(|&paid_at| paid_at > 0)
            }
            _ => None,
        };

        let network = self
            .get_network()
            .await
            .map(|network| Some(network.to_string()))
            .unwrap_or(None);

        // Use amount_received_msat if available (actual payment), fallback to amount_msat (invoice amount)
        let amount_sat = invoice
            .amount_received_msat
            .as_ref()
            .or(invoice.amount_msat.as_ref())
            .map(|amt| amt.msat / 1000)
            .unwrap_or(0);

        let amount_usd = self.price_converter.sats_to_usd(amount_sat).await?;

        let payment_hash_hex = hex::encode(&invoice.payment_hash);
        let htlcs = self
            .get_htlcs_for_payment(&payment_hash_hex)
            .await
            .unwrap_or_else(|_| vec![]);

        // For incoming payments, destination is our own node
        let destination_pubkey = Some(self.info.pubkey);

        Ok(PaymentDetails {
            state,
            payment_type: PaymentType::Incoming,
            amount_sat,
            amount_usd,
            routing_fee: None,
            network,
            description: invoice.description,
            creation_time,
            invoice: invoice.bolt11,
            payment_hash: payment_hash_hex,
            destination_pubkey,
            completed_at,
            htlcs,
        })
    }
}

async fn reader(filename: &str) -> Result<Vec<u8>, Error> {
    let mut file = File::open(filename).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;
    Ok(contents)
}

/// Unified interface for Lightning Network node operations across different implementations.
#[async_trait]
pub trait LightningClient: Send {
    /// Returns information about the node.
    fn get_info(&self) -> &NodeInfo;
    /// Retrieves the Bitcoin network the node is connected to.
    async fn get_network(&self) -> Result<Network, LightningError>;
    /// Lists all channels, returning only their capacities in millisatoshis.
    async fn list_channels(&self) -> Result<Vec<ChannelSummary>, LightningError>;
    /// Gets detailed information about a specific channel.
    async fn get_channel_info(
        &self,
        channel_id: &ShortChannelID,
    ) -> Result<ChannelDetails, LightningError>;
    /// Gets detailed information about a specific payment by its hash.
    async fn get_payment_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<PaymentDetails, LightningError>;
    async fn list_payments(&self) -> Result<Vec<PaymentSummary>, LightningError>;
    /// Returns a stream of raw events from the lightning node.
    async fn stream_events(
        &mut self,
    ) -> Result<Pin<Box<dyn Stream<Item = NodeSpecificEvent> + Send>>, LightningError>;
    /// Lists all invoices.
    async fn list_invoices(&self) -> Result<Vec<CustomInvoice>, LightningError>;
    /// Gets detailed information about a specific invoice by its payment hash.
    async fn get_invoice_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<CustomInvoice, LightningError>;
}

#[async_trait]
impl LightningClient for LndNode {
    /// Returns cached node information (node_id, alias, features) that was retrieved
    /// during node initialization. This avoids repeated RPC calls for static node data.
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    async fn get_network(&self) -> Result<Network, LightningError> {
        let mut client = self.client.lock().await;
        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        if info.chains.is_empty() {
            return Err(LightningError::ValidationError(format!(
                "{} is not connected any chain",
                self.get_info()
            )));
        } else if info.chains.len() > 1 {
            return Err(LightningError::ValidationError(format!(
                "{} is connected to more than one chain: {:?}",
                self.get_info(),
                info.chains.iter().map(|c| c.chain.to_string())
            )));
        }

        Ok(Network::from_str(match info.chains[0].network.as_str() {
            "mainnet" => "bitcoin",
            x => x,
        })
        .map_err(|err| LightningError::ValidationError(err.to_string()))?)
    }

    async fn list_channels(&self) -> Result<Vec<ChannelSummary>, LightningError> {
        let mut lightning_stub = self.get_lightning_stub().await;

        let list_channels_response = lightning_stub
            .list_channels(ListChannelsRequest::default())
            .await
            .map_err(|err| LightningError::ChannelError(err.to_string()))?
            .into_inner();

        let graph_response = lightning_stub
            .describe_graph(ChannelGraphRequest {
                include_unannounced: false,
            })
            .await
            .map_err(|err| LightningError::GetGraphError(err.to_string()))?
            .into_inner();

        let mut last_updates: HashMap<u64, u64> = HashMap::new();

        for edge in graph_response.edges.into_iter() {
            let mut max_last_update = 0u64;

            if let Some(node1_policy) = &edge.node1_policy {
                if node1_policy.last_update > 0 {
                    max_last_update = max_last_update.max(node1_policy.last_update as u64);
                }
            }

            if let Some(node2_policy) = &edge.node2_policy {
                if node2_policy.last_update > 0 {
                    max_last_update = max_last_update.max(node2_policy.last_update as u64);
                }
            }

            if max_last_update > 0 {
                let entry = last_updates.entry(edge.channel_id).or_insert(0);
                *entry = (*entry).max(max_last_update);
            }
        }

        let channels: Vec<ChannelSummary> = list_channels_response
            .channels
            .into_iter()
            .map(|channel| {
                let channel_state = if channel.active {
                    ChannelState::Active
                } else {
                    ChannelState::Disabled
                };

                let last_update = last_updates.get(&channel.chan_id).copied();

                ChannelSummary {
                    chan_id: ShortChannelID(channel.chan_id),
                    alias: None,
                    channel_state,
                    private: channel.private,
                    remote_balance: channel.remote_balance.try_into().unwrap_or(0),
                    local_balance: channel.local_balance.try_into().unwrap_or(0),
                    capacity: channel.capacity.try_into().unwrap_or(0),
                    last_update,
                    uptime: Some(channel.uptime as u64),
                }
            })
            .collect();

        Ok(channels)
    }

    async fn get_channel_info(
        &self,
        channel_id: &ShortChannelID,
    ) -> Result<ChannelDetails, LightningError> {
        let mut lightning_stub = self.get_lightning_stub().await;

        // Fetch basic channel info
        let response = lightning_stub
            .list_channels(ListChannelsRequest {
                active_only: false,
                ..Default::default()
            })
            .await
            .map_err(|err| {
                LightningError::ChannelError(format!("LND list_channels error: {err}"))
            })?;

        let channel_opt = response
            .into_inner()
            .channels
            .into_iter()
            .find(|channel| channel.chan_id == channel_id.0);

        match channel_opt {
            Some(channel) => {
                let channel_point = parse_channel_point(&channel.channel_point)?;
                let remote_pubkey = PublicKey::from_str(&channel.remote_pubkey).map_err(|err| {
                    LightningError::ChannelError(format!("Invalid remote pubkey: {err}"))
                })?;

                // Get policies from describe_graph
                let (node1_policy, node2_policy) = match lightning_stub
                    .describe_graph(ChannelGraphRequest {
                        include_unannounced: false,
                    })
                    .await
                {
                    Ok(graph_response) => {
                        let edges = graph_response.into_inner().edges;
                        if let Some(channel_edge) = edges
                            .into_iter()
                            .find(|channel_edge| channel_edge.channel_id == channel_id.0)
                        {
                            let node1_pubkey = PublicKey::from_str(&channel_edge.node1_pub)
                                .unwrap_or(remote_pubkey);
                            let node2_pubkey = PublicKey::from_str(&channel_edge.node2_pub)
                                .unwrap_or(self.info.pubkey);

                            let node1_policy =
                                channel_edge.node1_policy.as_ref().map(|routing_policy| {
                                    NodePolicy {
                                        pubkey: node1_pubkey,
                                        fee_base_msat: routing_policy.fee_base_msat as u64,
                                        fee_rate_milli_msat: routing_policy.fee_rate_milli_msat
                                            as u64,
                                        min_htlc_msat: routing_policy.min_htlc as u64,
                                        max_htlc_msat: if routing_policy.max_htlc_msat > 0 {
                                            Some(routing_policy.max_htlc_msat)
                                        } else {
                                            None
                                        },
                                        time_lock_delta: routing_policy.time_lock_delta as u16,
                                        disabled: routing_policy.disabled,
                                        last_update: Some(routing_policy.last_update as u64),
                                    }
                                });

                            let node2_policy =
                                channel_edge.node2_policy.as_ref().map(|routing_policy| {
                                    NodePolicy {
                                        pubkey: node2_pubkey,
                                        fee_base_msat: routing_policy.fee_base_msat as u64,
                                        fee_rate_milli_msat: routing_policy.fee_rate_milli_msat
                                            as u64,
                                        min_htlc_msat: routing_policy.min_htlc as u64,
                                        max_htlc_msat: if routing_policy.max_htlc_msat > 0 {
                                            Some(routing_policy.max_htlc_msat)
                                        } else {
                                            None
                                        },
                                        time_lock_delta: routing_policy.time_lock_delta as u16,
                                        disabled: routing_policy.disabled,
                                        last_update: Some(routing_policy.last_update as u64),
                                    }
                                });

                            (node1_policy, node2_policy)
                        } else {
                            (None, None)
                        }
                    }
                    Err(_) => (None, None),
                };

                Ok(ChannelDetails {
                    channel_id: ShortChannelID(channel.chan_id),
                    local_balance_sat: channel.local_balance.try_into().unwrap_or(0),
                    remote_balance_sat: channel.remote_balance.try_into().unwrap_or(0),
                    capacity_sat: channel.capacity.try_into().unwrap_or(0),
                    active: Some(channel.active),
                    private: channel.private,
                    remote_pubkey,
                    commit_fee_sat: Some(channel.commit_fee as u64),
                    local_chan_reserve_sat: Some(
                        channel
                            .local_constraints
                            .as_ref()
                            .map(|local_constraints| local_constraints.chan_reserve_sat)
                            .unwrap_or(0),
                    ),
                    remote_chan_reserve_sat: Some(
                        channel
                            .remote_constraints
                            .as_ref()
                            .map(|remote_constraints| remote_constraints.chan_reserve_sat)
                            .unwrap_or(0),
                    ),
                    num_updates: Some(channel.num_updates),
                    total_satoshis_sent: Some(channel.total_satoshis_sent as u64),
                    total_satoshis_received: Some(channel.total_satoshis_received as u64),
                    channel_age_blocks: channel.lifetime.try_into().ok(),
                    opening_cost_sat: None,
                    initiator: Some(channel.initiator),
                    txid: Some(channel_point.txid),
                    vout: Some(channel_point.vout),
                    node1_policy,
                    node2_policy,
                })
            }
            None => Err(LightningError::ChannelError(
                "Channel not found".to_string(),
            )),
        }
    }

    async fn get_payment_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<PaymentDetails, LightningError> {
        let mut lightning_stub = self.get_lightning_stub().await;
        let hex_hash = hex::encode(payment_hash.0);

        // Check if it's an outgoing payment
        let payments_response = lightning_stub
            .list_payments(ListPaymentsRequest {
                include_incomplete: true,
                ..Default::default()
            })
            .await
            .map_err(|err| {
                tracing::error!("list_payments RPC failed: {}", err);
                LightningError::PaymentError(format!("LND list_payments error: {err}"))
            })?
            .into_inner();

        if let Some(payment) = payments_response
            .payments
            .into_iter()
            .find(|payment| payment.payment_hash == hex_hash)
        {
            return self.process_outgoing_payment(payment).await;
        }

        // If it's not an outgoing payment, check if it's an incoming payment (invoice)
        let invoices_response = lightning_stub
            .list_invoices(ListInvoiceRequest::default())
            .await
            .map_err(|err| {
                tracing::error!("list_invoices RPC failed: {}", err);
                LightningError::InvoiceError(format!("LND list_invoices error: {err}"))
            })?
            .into_inner();

        if let Some(invoice) = invoices_response
            .invoices
            .into_iter()
            .find(|invoice| hex::encode(&invoice.r_hash) == hex_hash)
        {
            return self.process_incoming_payment(invoice).await;
        }

        Err(LightningError::NotFound(format!(
            "Payment {hex_hash} not found"
        )))
    }

    async fn list_payments(&self) -> Result<Vec<PaymentSummary>, LightningError> {
        let mut lightning_stub = self.get_lightning_stub().await;
        let btc_price = self.price_converter.fetch_btc_price().await?;

        // Fetch outgoing payments
        let payments_response = lightning_stub
            .list_payments(ListPaymentsRequest::default())
            .await
            .map_err(|err| LightningError::PaymentError(err.to_string()))?
            .into_inner();

        // Fetch incoming invoices
        let invoices_response = lightning_stub
            .list_invoices(ListInvoiceRequest::default())
            .await
            .map_err(|err| LightningError::InvoiceError(err.to_string()))?
            .into_inner();

        // Process outgoing payments
        let outgoing_payments: Vec<PaymentSummary> = payments_response
            .payments
            .into_iter()
            .filter_map(|payment| {
                let status =
                    PaymentStatus::try_from(payment.status).unwrap_or(PaymentStatus::Unknown);
                let state = match status {
                    PaymentStatus::Unknown | PaymentStatus::InFlight => PaymentState::Inflight,
                    PaymentStatus::Succeeded => PaymentState::Settled,
                    PaymentStatus::Failed => PaymentState::Failed,
                };

                let amount_sat: u64 = payment.value_sat.try_into().unwrap_or(0);
                let amount_usd = PriceConverter::sats_to_usd_with_price(amount_sat, btc_price);

                // Only set completed_at if payment succeeded
                let completed_at = match state {
                    PaymentState::Settled => payment
                        .htlcs
                        .last()
                        .map(|htlc| (htlc.resolve_time_ns / 1_000_000_000) as u64),
                    _ => None,
                };

                // Only set creation_time if timestamp is valid
                let creation_time = (payment.creation_time_ns > 0).then_some({
                    let creation_time_ns = payment.creation_time_ns as u64;
                    creation_time_ns / 1_000_000_000
                });

                Some(PaymentSummary {
                    state,
                    payment_type: PaymentType::Outgoing,
                    amount_sat,
                    amount_usd,
                    routing_fee: if payment.fee_sat > 0 {
                        Some(payment.fee_sat as u64)
                    } else {
                        None
                    },
                    creation_time,
                    invoice: Some(payment.payment_request),
                    payment_hash: payment.payment_hash,
                    completed_at,
                })
            })
            .collect();

        // Process incoming payments (from invoices)
        let incoming_payments: Vec<PaymentSummary> = invoices_response
            .invoices
            .into_iter()
            .filter(|invoice| {
                // Exclude invoices without payment attempts (HTLCs)
                !invoice.htlcs.is_empty()
            })
            .filter_map(|invoice| {
                let state = match invoice.state {
                    0 => PaymentState::Inflight,
                    1 => PaymentState::Settled,
                    2 => PaymentState::Failed,
                    3 => PaymentState::Inflight,
                    _ => return None,
                };

                // Use amt_paid_sat if available, fallback to invoice.value for failed attempts
                let amount_sat = if invoice.amt_paid_sat > 0 {
                    invoice.amt_paid_sat as u64
                } else {
                    invoice.value as u64
                };

                let amount_usd = PriceConverter::sats_to_usd_with_price(amount_sat, btc_price);

                let creation_time =
                    (invoice.creation_date > 0).then_some(invoice.creation_date as u64);

                let completed_at = match state {
                    PaymentState::Settled | PaymentState::Failed => {
                        (invoice.settle_date > 0).then_some(invoice.settle_date as u64)
                    }
                    _ => None,
                };

                Some(PaymentSummary {
                    state,
                    payment_type: PaymentType::Incoming,
                    amount_sat,
                    amount_usd,
                    routing_fee: None,
                    creation_time,
                    invoice: Some(invoice.payment_request),
                    payment_hash: hex::encode(invoice.r_hash),
                    completed_at,
                })
            })
            .collect();

        // Combine all with deduplication
        let mut seen_hashes = HashSet::new();
        let mut all_payments = Vec::new();

        let mut push_unique = |payment: PaymentSummary| {
            if seen_hashes.insert(payment.payment_hash.clone()) {
                all_payments.push(payment);
            }
        };

        outgoing_payments.into_iter().for_each(&mut push_unique);
        incoming_payments.into_iter().for_each(&mut push_unique);

        // Sort by creation time
        all_payments
            .sort_by(|outgoing, incoming| incoming.creation_time.cmp(&outgoing.creation_time));

        Ok(all_payments)
    }

    async fn stream_events(
        &mut self,
    ) -> Result<Pin<Box<dyn Stream<Item = NodeSpecificEvent> + Send>>, LightningError> {
        let channel_events_stream = self.stream_channel_events().await?;
        let invoice_events_stream = self.stream_invoice_events().await?;

        let event_stream = stream! {
            let channel_events_filtered = channel_events_stream.filter_map(|result| {
                let event_opt = match result {
                    Ok(update) => {
                        match update.r#type() {
                            LndChannelUpdateType::OpenChannel => {
                                if let Some(event_channel) = update.channel {
                                    match event_channel {
                                        EventChannel::OpenChannel(chan) => {
                                            Some(NodeSpecificEvent::LND(LNDEvent::ChannelOpened {
                                                active: chan.active,
                                                remote_pubkey: chan.remote_pubkey,
                                                channel_point: chan.channel_point,
                                                chan_id: chan.chan_id,
                                                capacity: chan.capacity,
                                                local_balance: chan.local_balance,
                                                remote_balance: chan.remote_balance,
                                                total_satoshis_sent: chan.total_satoshis_sent,
                                                total_satoshis_received: chan.total_satoshis_received,
                                            }))
                                        }
                                        _ => {
                                            eprintln!("Unexpected channel variant for OpenChannel event");
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            },
                            LndChannelUpdateType::ClosedChannel => {
                                if let Some(event_channel) = update.channel {
                                    match event_channel {
                                        EventChannel::ClosedChannel(chan_close_sum) => {
                                            Some(NodeSpecificEvent::LND(LNDEvent::ChannelClosed {
                                                channel_point: chan_close_sum.channel_point,
                                                chan_id:  chan_close_sum.chan_id,
                                                chain_hash:  chan_close_sum.chain_hash,
                                                closing_tx_hash:  chan_close_sum.closing_tx_hash,
                                                remote_pubkey:  chan_close_sum.remote_pubkey,
                                                capacity:  chan_close_sum.capacity,
                                                close_height:  chan_close_sum.close_height,
                                                settled_balance:  chan_close_sum.settled_balance,
                                                time_locked_balance:  chan_close_sum.time_locked_balance,
                                                close_type:  chan_close_sum.close_type,
                                                open_initiator:  chan_close_sum.open_initiator,
                                                close_initiator:  chan_close_sum.close_initiator,
                                            }))
                                        }
                                        _ => {
                                            eprintln!("Unexpected channel variant for ClosedChannel event");
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            },
                            _ => None,
                        }
                    }
                    Err(e) => {
                        eprintln!("Error receiving LND channel event: {e:?}");
                        None
                    }
                };
                futures::future::ready(event_opt)
            });

            let invoice_events_filtered = invoice_events_stream.filter_map(|result| {
                let event_opt = match result {
                    Ok(invoice) => {
                        match invoice.state() {
                            InvoiceState::Open => {
                                Some(NodeSpecificEvent::LND(LNDEvent::InvoiceCreated {
                                        preimage: invoice.r_preimage,
                                        hash: invoice.r_hash,
                                        value_msat: invoice.value_msat,
                                        state: invoice.state,
                                        memo: invoice.memo,
                                        creation_date: invoice.creation_date,
                                        payment_request: invoice.payment_request,
                                }))
                            },
                            InvoiceState::Settled => {
                                  Some(NodeSpecificEvent::LND(LNDEvent::InvoiceSettled {
                                        preimage: invoice.r_preimage,
                                        hash: invoice.r_hash,
                                        value_msat: invoice.value_msat,
                                        state: invoice.state,
                                        memo: invoice.memo,
                                        creation_date: invoice.creation_date,
                                        payment_request: invoice.payment_request,
                                }))
                            },
                            InvoiceState::Canceled => {
                                  Some(NodeSpecificEvent::LND(LNDEvent::InvoiceCancelled {
                                        preimage: invoice.r_preimage,
                                        hash: invoice.r_hash,
                                        value_msat: invoice.value_msat,
                                        state: invoice.state,
                                        memo: invoice.memo,
                                        creation_date: invoice.creation_date,
                                        payment_request: invoice.payment_request,
                                }))
                            },
                            InvoiceState::Accepted => {
                                  Some(NodeSpecificEvent::LND(LNDEvent::InvoiceAccepted {
                                        preimage: invoice.r_preimage,
                                        hash: invoice.r_hash,
                                        value_msat: invoice.value_msat,
                                        state: invoice.state,
                                        memo: invoice.memo,
                                        creation_date: invoice.creation_date,
                                        payment_request: invoice.payment_request,
                                }))
                            }
                        }
                    },
                    Err(e) => {
                        eprintln!("Error subscribing to LND channel events: {e:?}");
                        None
                    }
                };
                futures::future::ready(event_opt)
            });

            let mut merged_stream = SelectAll::new();
            merged_stream.push(channel_events_filtered.boxed());
            merged_stream.push(invoice_events_filtered.boxed());

            while let Some(event) = merged_stream.next().await {
                yield event;
            }
        };

        Ok(Box::pin(event_stream))
    }

    async fn list_invoices(&self) -> Result<Vec<CustomInvoice>, LightningError> {
        let mut client = self.client.lock().await;
        let request = tonic_lnd::lnrpc::ListInvoiceRequest {
            pending_only: false,
            ..Default::default()
        };

        let response = client
            .lightning()
            .list_invoices(request)
            .await
            .map_err(|err| LightningError::InvoiceError(err.to_string()))?
            .into_inner();

        let invoices = response
            .invoices
            .into_iter()
            .map(|invoice| {
                // Map tonic's InvoiceState to your InvoiceStatus enum
                let state =
                    match InvoiceState::try_from(invoice.state).unwrap_or(InvoiceState::Open) {
                        InvoiceState::Open => InvoiceStatus::Open,
                        InvoiceState::Settled => InvoiceStatus::Settled,
                        InvoiceState::Canceled => InvoiceStatus::Failed,
                        InvoiceState::Accepted => InvoiceStatus::Open,
                    };
                let htlcs = Some(
                    invoice
                        .htlcs
                        .into_iter()
                        .map(|htlc| InvoiceHtlc {
                            chan_id: Some(htlc.chan_id),
                            htlc_index: Some(htlc.htlc_index),
                            amt_msat: Some(htlc.amt_msat),
                            accept_time: Some(htlc.accept_time),
                            resolve_time: Some(htlc.resolve_time),
                            expiry_height: htlc.expiry_height.try_into().ok(),
                            mpp_total_amt_msat: Some(htlc.mpp_total_amt_msat),
                        })
                        .collect(),
                );

                let features = Some(
                    invoice
                        .features
                        .into_iter()
                        .map(|(feature_bit, feature_entry)| {
                            (
                                feature_bit,
                                Feature {
                                    name: Some(feature_entry.name),
                                    is_known: Some(feature_entry.is_known),
                                    is_required: Some(feature_entry.is_required),
                                },
                            )
                        })
                        .collect(),
                );

                CustomInvoice {
                    memo: invoice.memo,
                    payment_hash: hex::encode(invoice.r_hash),
                    payment_preimage: Some(hex::encode(invoice.r_preimage))
                        .filter(|preimage_hex| !preimage_hex.is_empty())
                        .unwrap_or_default(),
                    value: invoice.value as u64,
                    value_msat: invoice.value_msat as u64,
                    creation_date: Some(invoice.creation_date),
                    settle_date: Some(invoice.settle_date),
                    payment_request: invoice.payment_request,
                    expiry: Some(invoice.expiry as u64),
                    state,
                    is_keysend: Some(invoice.is_keysend),
                    is_amp: Some(invoice.is_amp),
                    payment_addr: Some(hex::encode(invoice.payment_addr))
                        .filter(|addr_hex| !addr_hex.is_empty()),
                    htlcs,
                    features,
                }
            })
            .collect();

        Ok(invoices)
    }

    async fn get_invoice_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<CustomInvoice, LightningError> {
        let mut client = self.get_lightning_stub().await;

        let request = tonic_lnd::lnrpc::PaymentHash {
            r_hash: payment_hash.0.to_vec(),
            ..Default::default()
        };

        let response = client
            .lookup_invoice(request)
            .await
            .map_err(|e| LightningError::InvoiceError(e.to_string()))?
            .into_inner();

        let state = match InvoiceState::try_from(response.state).unwrap_or(InvoiceState::Open) {
            InvoiceState::Open => InvoiceStatus::Open,
            InvoiceState::Settled => InvoiceStatus::Settled,
            InvoiceState::Canceled => InvoiceStatus::Failed,
            InvoiceState::Accepted => InvoiceStatus::Open,
        };

        Ok(CustomInvoice {
            memo: response.memo,
            payment_hash: hex::encode(response.r_hash),
            payment_preimage: Some(hex::encode(response.r_preimage))
                .filter(|preimage_hex| !preimage_hex.is_empty())
                .unwrap_or_default(),
            value: response.value as u64,
            value_msat: response.value_msat as u64,
            creation_date: Some(response.creation_date),
            settle_date: Some(response.settle_date),
            payment_request: response.payment_request,
            expiry: Some(response.expiry as u64),
            state,
            is_keysend: Some(response.is_keysend),
            is_amp: Some(response.is_amp),
            payment_addr: Some(hex::encode(response.payment_addr))
                .filter(|addr_hex| !addr_hex.is_empty()),
            htlcs: None,
            features: None,
        })
    }
}

#[async_trait]
impl LightningClient for ClnNode {
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    async fn get_network(&self) -> Result<Network, LightningError> {
        let mut client = self.client.lock().await;
        let info = client
            .getinfo(GetinfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        Ok(Network::from_core_arg(&info.network)
            .map_err(|err| LightningError::ValidationError(err.to_string()))?)
    }

    async fn list_channels(&self) -> Result<Vec<ChannelSummary>, LightningError> {
        let mut client = self.get_client_stub().await;

        // Get basic channel data
        let peer_channels_response = client
            .list_peer_channels(ListpeerchannelsRequest { id: None })
            .await
            .map_err(|err| LightningError::ChannelError(err.to_string()))?
            .into_inner();

        // Get routing info
        let routing_channels_response = client
            .list_channels(ListchannelsRequest::default())
            .await
            .map_err(|err| LightningError::ChannelError(format!("Failed to list channels: {err}")))?
            .into_inner();

        let mut channel_routing_info = HashMap::new();
        for routing_channel in routing_channels_response.channels {
            channel_routing_info
                .entry(routing_channel.short_channel_id)
                .and_modify(|info: &mut (u64, bool)| {
                    info.0 = info.0.max(routing_channel.last_update as u64);
                    info.1 |= routing_channel.public;
                })
                .or_insert((routing_channel.last_update as u64, routing_channel.public));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let channel_summaries = peer_channels_response
            .channels
            .into_iter()
            .filter_map(|peer_channel| {
                let short_channel_id_str = peer_channel.short_channel_id.as_ref()?;
                let channel_id = short_channel_id_str.parse().ok()?;

                let capacity_satoshis: u64 = peer_channel
                    .total_msat
                    .as_ref()
                    .map(|amt| amt.msat)
                    .unwrap_or(0)
                    / 1000;
                let local_balance_satoshis: u64 = peer_channel
                    .to_us_msat
                    .as_ref()
                    .map(|amt| amt.msat)
                    .unwrap_or(0)
                    / 1000;
                let remote_balance_satoshis =
                    capacity_satoshis.saturating_sub(local_balance_satoshis);

                let channel_state = match peer_channel.state {
                    0 | 1 | 9 | 10 => ChannelState::Opening,
                    2 => ChannelState::Active,
                    3..=5 => ChannelState::Closing,
                    8 => ChannelState::Closed,
                    _ => ChannelState::Disabled,
                };

                let alias = peer_channel.alias.as_ref().and_then(|a| a.remote.clone());

                // Get routing info if available
                let (last_update_timestamp, is_public) = channel_routing_info
                    .get(short_channel_id_str)
                    .copied()
                    .unwrap_or((0, false));

                // For private channels with no routing update, use current time as fallback
                let last_update_timestamp = if !is_public && last_update_timestamp == 0 {
                    now
                } else {
                    last_update_timestamp
                };

                Some(ChannelSummary {
                    chan_id: channel_id,
                    alias,
                    channel_state,
                    private: !is_public,
                    remote_balance: remote_balance_satoshis,
                    local_balance: local_balance_satoshis,
                    capacity: capacity_satoshis,
                    last_update: Some(last_update_timestamp),
                    uptime: None,
                })
            })
            .collect();

        Ok(channel_summaries)
    }

    async fn get_channel_info(
        &self,
        channel_id: &ShortChannelID,
    ) -> Result<ChannelDetails, LightningError> {
        let mut client = self.get_client_stub().await;
        let channel = client
            .list_peer_channels(ListpeerchannelsRequest { id: None })
            .await
            .map_err(|err| {
                LightningError::ChannelError(format!("Failed to list peer channels: {err}"))
            })?
            .into_inner()
            .channels
            .into_iter()
            .find(|channel| channel.short_channel_id.as_deref() == Some(&channel_id.0.to_string()))
            .ok_or_else(|| {
                LightningError::ChannelError(format!("Channel {channel_id} not found"))
            })?;

        // Get additional info from list_channels
        let list_channels_response = client
            .list_channels(ListchannelsRequest {
                short_channel_id: Some(channel_id.0.to_string()),
                ..Default::default()
            })
            .await
            .map_err(|err| LightningError::ChannelError(format!("Failed to list channels: {err}")))?
            .into_inner();

        let remote_pubkey = PublicKey::from_slice(&channel.peer_id).map_err(|err| {
            LightningError::ChannelError(format!(
                "Invalid peer pubkey for channel {channel_id}: {err}"
            ))
        })?;

        // Extract last_update for both directions
        let mut local_last_update = None;
        let mut remote_last_update = None;
        let mut is_active_option = None;

        for channel in &list_channels_response.channels {
            // Convert Vec<u8> to String before parsing as pubkey
            if let Ok(source_str) = String::from_utf8(channel.source.clone()) {
                if let Ok(pubkey) = PublicKey::from_str(&source_str) {
                    let update_time = Some(channel.last_update as u64);
                    if pubkey == self.info.pubkey {
                        local_last_update = update_time;
                        is_active_option = Some(channel.active);
                    } else if pubkey == remote_pubkey {
                        remote_last_update = update_time;
                    }
                }
            }
        }

        let is_active = is_active_option.unwrap_or(false);

        let capacity_sat = channel
            .total_msat
            .as_ref()
            .ok_or(LightningError::ChannelError(format!(
                "Missing total_msat for channel {}",
                channel_id
            )))?
            .msat
            / 1000;

        let local_balance_sat = channel
            .to_us_msat
            .as_ref()
            .ok_or(LightningError::ChannelError(format!(
                "Missing to_us_msat for channel {channel_id}"
            )))?
            .msat
            / 1000;

        let remote_balance_sat =
            capacity_sat
                .checked_sub(local_balance_sat)
                .ok_or(LightningError::ChannelError(format!(
                    "Invalid balance calculation for channel {channel_id}"
                )))?;

        let initiator = match channel.opener().as_str_name() {
            "LOCAL" => Some(true),
            "REMOTE" => Some(false),
            _ => None,
        };

        let updates = channel
            .updates
            .as_ref()
            .ok_or(LightningError::ChannelError(format!(
                "Missing channel updates for channel {channel_id}"
            )))?;

        let local_policy = updates
            .local
            .as_ref()
            .ok_or(LightningError::ChannelError(format!(
                "Missing local policy for channel {channel_id}"
            )))?;

        let remote_policy =
            updates
                .remote
                .as_ref()
                .ok_or(LightningError::ChannelError(format!(
                    "Missing remote policy for channel {channel_id}"
                )))?;

        // Build policy structs
        let local_policy_struct = NodePolicy {
            pubkey: self.info.pubkey,
            fee_base_msat: local_policy
                .fee_base_msat
                .as_ref()
                .ok_or(LightningError::ChannelError(format!(
                    "Missing fee_base_msat in local policy for channel {channel_id}"
                )))?
                .msat,
            fee_rate_milli_msat: local_policy.fee_proportional_millionths as u64,
            min_htlc_msat: local_policy
                .htlc_minimum_msat
                .as_ref()
                .ok_or(LightningError::ChannelError(format!(
                    "Missing htlc_minimum_msat in local policy for channel {channel_id}"
                )))?
                .msat,
            max_htlc_msat: local_policy.htlc_maximum_msat.as_ref().map(|amt| amt.msat),
            time_lock_delta: local_policy.cltv_expiry_delta as u16,
            disabled: !is_active,
            last_update: local_last_update,
        };

        let remote_policy_struct = NodePolicy {
            pubkey: remote_pubkey,
            fee_base_msat: remote_policy
                .fee_base_msat
                .as_ref()
                .ok_or(LightningError::ChannelError(format!(
                    "Missing fee_base_msat in remote policy for channel {channel_id}"
                )))?
                .msat,
            fee_rate_milli_msat: remote_policy.fee_proportional_millionths as u64,
            min_htlc_msat: remote_policy
                .htlc_minimum_msat
                .as_ref()
                .ok_or(LightningError::ChannelError(format!(
                    "Missing htlc_minimum_msat in remote policy for channel {channel_id}"
                )))?
                .msat,
            max_htlc_msat: remote_policy.htlc_maximum_msat.as_ref().map(|amt| amt.msat),
            time_lock_delta: remote_policy.cltv_expiry_delta as u16,
            disabled: !is_active,
            last_update: remote_last_update,
        };

        // Determine policy ordering
        let (node1_policy, node2_policy) = if self.info.pubkey < remote_pubkey {
            (local_policy_struct, remote_policy_struct)
        } else {
            (remote_policy_struct, local_policy_struct)
        };

        // Handle txid conversion
        let txid = if let Some(txid_bytes) = channel.funding_txid.as_ref() {
            std::str::from_utf8(txid_bytes)
                .ok()
                .and_then(|txid_str| Txid::from_str(txid_str).ok())
        } else {
            None
        };

        Ok(ChannelDetails {
            channel_id: *channel_id,
            local_balance_sat,
            remote_balance_sat,
            capacity_sat,
            active: Some(is_active),
            private: channel.private.unwrap_or(false),
            remote_pubkey,
            commit_fee_sat: channel.last_tx_fee_msat.as_ref().map(|amt| amt.msat / 1000),
            local_chan_reserve_sat: channel.our_reserve_msat.as_ref().map(|amt| amt.msat / 1000),
            remote_chan_reserve_sat: channel
                .their_reserve_msat
                .as_ref()
                .map(|amt| amt.msat / 1000),
            num_updates: None,
            total_satoshis_sent: channel
                .out_fulfilled_msat
                .as_ref()
                .map(|amt| amt.msat / 1000),
            total_satoshis_received: channel
                .in_fulfilled_msat
                .as_ref()
                .map(|amt| amt.msat / 1000),
            channel_age_blocks: None,
            opening_cost_sat: None,
            initiator,
            txid,
            vout: channel.funding_outnum,
            node1_policy: Some(node1_policy),
            node2_policy: Some(node2_policy),
        })
    }
    async fn get_payment_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<PaymentDetails, LightningError> {
        let mut client = self.get_client_stub().await;
        let hex_hash = hex::encode(payment_hash.0);

        // Check if it's an outgoing payment
        let response = client
            .list_pays(cln_grpc::pb::ListpaysRequest {
                payment_hash: Some(payment_hash.0.to_vec()),
                ..Default::default()
            })
            .await
            .map_err(|err| LightningError::PaymentError(format!("CLN listpays error: {err}")))?
            .into_inner();

        if let Some(payment) = response.pays.into_iter().last() {
            return self.process_outgoing_payment(payment).await;
        }

        // If it's not an outgoing payment, check if it's an incoming payment (invoice)
        let invoice_response = client
            .list_invoices(cln_grpc::pb::ListinvoicesRequest::default())
            .await
            .map_err(|err| {
                tracing::error!("list_invoices RPC failed: {}", err);
                LightningError::InvoiceError(format!("CLN list_invoices error: {err}"))
            })?
            .into_inner();

        if let Some(invoice) = invoice_response
            .invoices
            .into_iter()
            .find(|invoice| hex::encode(&invoice.payment_hash) == hex_hash)
        {
            return self.process_incoming_payment(invoice).await;
        }

        Err(LightningError::NotFound(format!(
            "Payment {hex_hash} not found"
        )))
    }

    async fn list_payments(&self) -> Result<Vec<PaymentSummary>, LightningError> {
        let mut client = self.get_client_stub().await;
        let btc_price = self.price_converter.fetch_btc_price().await?;

        // Fetch outgoing payments
        let pays_response = client
            .list_pays(cln_grpc::pb::ListpaysRequest::default())
            .await
            .map_err(|err| LightningError::PaymentError(err.to_string()))?
            .into_inner();

        // Fetch incoming invoices
        let invoices_response = client
            .list_invoices(cln_grpc::pb::ListinvoicesRequest::default())
            .await
            .map_err(|err| LightningError::InvoiceError(err.to_string()))?
            .into_inner();

        // Process outgoing payments
        let outgoing_payments: Vec<PaymentSummary> = pays_response
            .pays
            .into_iter()
            .filter_map(|payment| {
                let state = match payment.status {
                    0 => PaymentState::Inflight, // pending
                    1 => PaymentState::Settled,  // complete
                    2 => PaymentState::Failed,   // failed
                    _ => PaymentState::Failed,
                };

                let amount_sat = payment
                    .amount_msat
                    .as_ref()
                    .map(|msat| msat.msat / 1000)
                    .unwrap_or(0);

                let amount_usd = PriceConverter::sats_to_usd_with_price(amount_sat, btc_price);

                let routing_fee = match (
                    payment.amount_sent_msat.as_ref(),
                    payment.amount_msat.as_ref(),
                ) {
                    (Some(sent), Some(received)) => Some((sent.msat - received.msat) / 1000),
                    _ => None,
                };

                let creation_time = (payment.created_at > 0).then_some(payment.created_at);

                Some(PaymentSummary {
                    state,
                    payment_type: PaymentType::Outgoing,
                    amount_sat,
                    amount_usd,
                    routing_fee,
                    creation_time,
                    invoice: payment.bolt11,
                    payment_hash: hex::encode(&payment.payment_hash),
                    completed_at: payment.completed_at,
                })
            })
            .collect();

        // Process incoming payments (from invoices)
        let incoming_payments: Vec<PaymentSummary> = invoices_response
            .invoices
            .into_iter()
            .filter(|invoice| {
                // Only include invoices with payment attempts
                invoice.pay_index.is_some()
            })
            .filter_map(|invoice| {
                let state = match invoice.status {
                    0 => PaymentState::Inflight, // unpaid
                    1 => PaymentState::Settled,  // paid
                    2 => PaymentState::Failed,   // expired
                    _ => return None,
                };

                // Use amount_received_msat if available (actual payment), fallback to amount_msat (invoice amount)
                let amount_sat = invoice
                    .amount_received_msat
                    .as_ref()
                    .or(invoice.amount_msat.as_ref())
                    .map(|amt| amt.msat / 1000)
                    .unwrap_or(0);

                let amount_usd = PriceConverter::sats_to_usd_with_price(amount_sat, btc_price);

                let creation_time = (invoice.expires_at > 0).then_some(invoice.expires_at);

                let completed_at = match state {
                    PaymentState::Settled | PaymentState::Failed => {
                        invoice.paid_at.filter(|&paid_at| paid_at > 0)
                    }
                    _ => None,
                };

                Some(PaymentSummary {
                    state,
                    payment_type: PaymentType::Incoming,
                    amount_sat,
                    amount_usd,
                    routing_fee: None,
                    creation_time,
                    invoice: invoice.bolt11,
                    payment_hash: hex::encode(&invoice.payment_hash),
                    completed_at,
                })
            })
            .collect();

        // Combine all with deduplication
        let mut seen_hashes = HashSet::new();
        let mut all_payments = Vec::new();

        let mut push_unique = |payment: PaymentSummary| {
            if seen_hashes.insert(payment.payment_hash.clone()) {
                all_payments.push(payment);
            }
        };

        outgoing_payments.into_iter().for_each(&mut push_unique);
        incoming_payments.into_iter().for_each(&mut push_unique);

        // Sort by creation time
        all_payments
            .sort_by(|outgoing, incoming| incoming.creation_time.cmp(&outgoing.creation_time));

        Ok(all_payments)
    }

    async fn stream_events(
        &mut self,
    ) -> Result<Pin<Box<dyn Stream<Item = NodeSpecificEvent> + Send>>, LightningError> {
        let event_stream = async_stream::stream! {
            let mut _counter = 0;
            loop {
                sleep(Duration::from_millis(60)).await;
                yield NodeSpecificEvent::CLN(CLNEvent::ChannelOpened {  });
                _counter += 1;
            }
        };

        Ok(Box::pin(event_stream))
    }

    async fn list_invoices(&self) -> Result<Vec<CustomInvoice>, LightningError> {
        let mut client = self.get_client_stub().await;
        let response = client
            .list_invoices(cln_grpc::pb::ListinvoicesRequest::default())
            .await
            .map_err(|err| LightningError::InvoiceError(err.to_string()))?
            .into_inner();

        let now = chrono::Utc::now().timestamp() as u64;

        let invoices = response
            .invoices
            .into_iter()
            .map(|invoice| {
                let amount_msat = invoice
                    .amount_msat
                    .as_ref()
                    .map(|amt_msat| amt_msat.msat)
                    .unwrap_or(0);
                let amount_sats = amount_msat / 1000;

                let expires_at = invoice.expires_at;

                let state = match invoice.status {
                    1 => InvoiceStatus::Settled, // paid
                    2 => InvoiceStatus::Expired, // expired
                    3 => InvoiceStatus::Failed,  // failed
                    _ => {
                        if invoice.expires_at <= now {
                            InvoiceStatus::Expired
                        } else {
                            InvoiceStatus::Open
                        }
                    }
                };

                CustomInvoice {
                    memo: invoice.description.unwrap_or_default(),
                    payment_hash: hex::encode(invoice.payment_hash),
                    payment_preimage: invoice
                        .payment_preimage
                        .map(hex::encode)
                        .unwrap_or_default(),
                    value: amount_sats,
                    value_msat: amount_msat,
                    creation_date: None,
                    settle_date: invoice.paid_at.map(|timestamp| timestamp as i64),
                    payment_request: invoice.bolt11.unwrap_or_default(),
                    expiry: Some(expires_at),
                    state,
                    is_keysend: None,
                    is_amp: None,
                    payment_addr: None,
                    htlcs: None,
                    features: None,
                }
            })
            .collect();

        Ok(invoices)
    }

    async fn get_invoice_details(
        &self,
        payment_hash: &PaymentHash,
    ) -> Result<CustomInvoice, LightningError> {
        let mut client = self.get_client_stub().await;

        let request = cln_grpc::pb::ListinvoicesRequest {
            payment_hash: Some(payment_hash.0.to_vec()),
            ..Default::default()
        };

        let response = client
            .list_invoices(request)
            .await
            .map_err(|e| LightningError::InvoiceError(format!("CLN listinvoices error: {e}")))?
            .into_inner();

        let invoice = response
            .invoices
            .into_iter()
            .next()
            .ok_or_else(|| LightningError::NotFound("Invoice not found".into()))?;

        let state = match invoice.status {
            1 => InvoiceStatus::Settled, // paid
            2 => InvoiceStatus::Expired, // expired
            3 => InvoiceStatus::Failed,  // failed
            _ => {
                let now = chrono::Utc::now().timestamp() as u64;

                if invoice.expires_at <= now {
                    InvoiceStatus::Expired
                } else {
                    InvoiceStatus::Open
                }
            }
        };

        let amount_msat = invoice
            .amount_msat
            .as_ref()
            .map(|amt_msat| amt_msat.msat)
            .unwrap_or(0);
        let amount_sats = amount_msat / 1000;

        Ok(CustomInvoice {
            memo: invoice.description.unwrap_or_default(),
            payment_hash: hex::encode(invoice.payment_hash),
            payment_preimage: invoice
                .payment_preimage
                .map(hex::encode)
                .unwrap_or_default(),
            value: amount_sats,
            value_msat: amount_msat,
            creation_date: None,
            settle_date: invoice.paid_at.map(|timestamp| timestamp as i64),
            payment_request: invoice.bolt11.unwrap_or_default(),
            expiry: Some(invoice.expires_at),
            state,
            is_keysend: None,
            is_amp: None,
            payment_addr: None,
            htlcs: None,
            features: None,
        })
    }
}
pub fn parse_channel_point(channel_point_str: &str) -> Result<OutPoint, LightningError> {
    let mut parts = channel_point_str.split(':');
    let txid_str = parts
        .next()
        .ok_or_else(|| LightningError::ValidationError("Missing txid".into()))?;
    let vout_str = parts
        .next()
        .ok_or_else(|| LightningError::ValidationError("Missing vout".into()))?;

    let txid = Txid::from_str(txid_str)
        .map_err(|err| LightningError::ValidationError(format!("Invalid txid: {err}")))?;
    let vout = vout_str
        .parse::<u32>()
        .map_err(|err| LightningError::ValidationError(format!("Invalid vout: {err}")))?;

    Ok(OutPoint { txid, vout })
}
