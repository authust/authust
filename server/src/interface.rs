use std::future::{ready, Ready};

use axum::{routing::get_service, Router};
use http::StatusCode;
use tower_http::services::{ServeDir, ServeFile};

use crate::SharedState;

use self::flow::setup_flow_router;

mod flow;

pub fn setup_interface_router() -> Router<SharedState> {
    let spa = spa_router();
    Router::new()
        .nest("/flow/-", setup_flow_router())
        .merge(spa)
}

fn handle_error(_err: std::io::Error) -> Ready<StatusCode> {
    ready(StatusCode::INTERNAL_SERVER_ERROR)
}

fn spa_router() -> Router<SharedState> {
    let asset_service = get_service(ServeDir::new("dist/assets")).handle_error(handle_error);
    let favicon_service =
        get_service(ServeFile::new("dist/favicon.ico")).handle_error(handle_error);
    let fallback_service =
        get_service(ServeFile::new("dist/index.html")).handle_error(handle_error);
    Router::new()
        .nest_service("/assets", asset_service)
        .route_service("/favicon.ico", favicon_service)
        .fallback_service(fallback_service)
}