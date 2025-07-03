//! Main entry point for the NodeGaze backend.
//!
//! This file initializes the Axum web server, sets up database connections,
//! and registers all API routes and middleware.
//! It orchestrates the application's startup and defines its overall structure.

mod utils;
mod errors;
mod services;
mod api;

use axum::{routing::get, Router, response::Json};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(root_handler))
        .nest("/api/node", api::node::routes::node_router().await);
    
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn root_handler() -> Json<Value> {
    Json(json!({ "message": "Welcome to  NodeGaze" }))
}
