//! Defines the HTTP routes for accessing node observability data.
//!
//! These routes map specific API paths to handler functions responsible for
//! serving channel statistics, node events, and other lightning-related information.

use axum::{routing::{post, get}, Router};
use super::handlers::{authenticate_node, get_node_info};

pub async fn node_router() -> Router {
    let app = Router::new()
        .route("/auth", post(authenticate_node))
        .route("/info", get(get_node_info));
    app
}