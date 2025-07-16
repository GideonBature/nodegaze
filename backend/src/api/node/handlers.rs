//! Handler functions for the node observability API.
//!
//! These functions process requests for lightning data, interact with the
//! `services::node_manager` and `services::data_aggregator` to retrieve and
//! process information, and format the responses.

use crate::services::node_manager::{ConnectionRequest, LndNode, ClnNode};
use crate::utils::NodeInfo;
use axum::{extract::Json, http::StatusCode};
use crate::services::node_manager::LightningClient;
use crate::errors::LightningError;

#[axum::debug_handler]
pub async fn authenticate_node(
    Json(payload): Json<ConnectionRequest>,
) -> Result<Json<NodeInfo>, (StatusCode, String)> {
    match payload {
        ConnectionRequest::Lnd(lnd_conn) => {
            tracing::info!("Attempting to authenticate LND node: {:?}", lnd_conn.id);
            match LndNode::new(lnd_conn).await {
                Ok(lnd_node) => {
                    tracing::info!("LND node authenticated: {:?}", lnd_node.info);
                    Ok(Json(lnd_node.info))
                }
                Err(e) => {
                    tracing::error!("Failed to authenticate LND node: {}", e);
                    Err((StatusCode::INTERNAL_SERVER_ERROR, format!("LND authentication failed: {}", e)))
                }
            }
        }
        ConnectionRequest::Cln(cln_conn) => {
            tracing::info!("Attempting to authenticate CLN node: {:?}", cln_conn.id);
            match ClnNode::new(cln_conn).await {
                Ok(cln_node) => {
                    tracing::info!("CLN node authenticated: {:?}", cln_node.info);
                    Ok(Json(cln_node.info))
                }
                Err(e) => {
                    tracing::error!("Failed to authenticate CLN node: {}", e);
                    Err((StatusCode::INTERNAL_SERVER_ERROR, format!("CLN authentication failed: {}", e)))
                }
            }
        }
    }
}

pub async fn connect_lightning(
    conn: ConnectionRequest,
) -> Result<Box<dyn LightningClient + Send>, LightningError> {
    match conn {
        ConnectionRequest::Lnd(lnd_conn) => {
            let node = LndNode::new(lnd_conn).await?;
            Ok(Box::new(node))
        }
        ConnectionRequest::Cln(cln_conn) => {
            let node = ClnNode::new(cln_conn).await?;
            Ok(Box::new(node))
        }
    }
}

#[axum::debug_handler]
pub async fn get_node_info(
    Json(payload): Json<ConnectionRequest>,
) -> Result<Json<NodeInfo>, (StatusCode, String)> {
    match connect_lightning(payload).await {
        Ok(client) => Ok(Json(client.get_info().clone())),
        Err(e) => {
            tracing::error!("Failed to get node info: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}
