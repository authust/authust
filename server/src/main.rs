use std::ops::DerefMut;

use std::sync::Arc;
use std::time::Duration;

use crate::api::setup_api_v1;
use crate::config::PostgresConfiguration;
use crate::config::{AuthustConfiguration, InternalAuthustConfiguration};
use crate::interface::setup_interface_router;
use crate::otel_middleware::{otel_layer, ExtensionLayer};
use crate::service::user::UserService;
use api::AuthServiceData;

use axum::error_handling::HandleErrorLayer;
use axum::extract::FromRef;
use axum::routing::get;
use axum::{BoxError, Router};
use deadpool_postgres::{GenericClient, Manager, ManagerConfig, Object, Pool, PoolError};

use executor::FlowExecutor;
use futures::{Future, FutureExt};
use http::StatusCode;
use jsonwebtoken::{DecodingKey, EncodingKey};
use model::{Flow, Policy, Prompt, Stage, Tenant, TenantQuery};

use opentelemetry::sdk::resource::{EnvResourceDetector, ResourceDetector};
use opentelemetry::{sdk::Resource, Key, KeyValue};
use opentelemetry_otlp::{self as otlp, ExportConfig};

use otlp::{SpanExporterBuilder, TonicExporterBuilder, WithExportConfig};
use service::policy::PolicyService;

use storage::datacache::{Data, DataStorage};
use storage::{StorageError, StorageManager};
use tokio::signal;
use tower::ServiceBuilder;
use tower_cookies::CookieManagerLayer;
use tracing::info;
use tracing_error::ErrorLayer;
use tracing_log::LogTracer;
use tracing_subscriber::{prelude::*, EnvFilter};

pub mod api;
pub mod auth;
pub mod config;
pub mod executor;
pub mod interface;
mod otel_middleware;
pub mod service;

#[tokio::main]
async fn main() {
    setup().await;
}

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("./migrations/");
}

fn setup_grpc_exporter() -> TonicExporterBuilder {
    otlp::new_exporter().tonic().with_env()
}

#[cfg_attr(not(feature = "otlp-http-proto"), allow(unused))]
fn setup_span_exporter(config: &ExportConfig) -> SpanExporterBuilder {
    #[cfg(not(feature = "otlp-http-proto"))]
    return setup_grpc_exporter().into();

    #[cfg(feature = "otlp-http-proto")]
    return match config.protocol {
        opentelemetry_otlp::Protocol::Grpc => setup_grpc_exporter().into(),
        opentelemetry_otlp::Protocol::HttpBinary => otlp::new_exporter().http().with_env().into(),
    };
}

struct ServiceNameDetector;

impl ResourceDetector for ServiceNameDetector {
    fn detect(&self, _timeout: Duration) -> Resource {
        Resource::new(vec![KeyValue::new(
            "service.name",
            std::env::var("OTEL_SERVICE_NAME")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    EnvResourceDetector::new()
                        .detect(Duration::from_secs(0))
                        .get(Key::new("service.name"))
                        .map(|v| v.to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| env!("CARGO_BIN_NAME").to_string())
                }),
        )])
    }
}

async fn setup() {
    if std::env::var("RUST_LOG").ok().is_none() {
        std::env::set_var("RUST_LOG", "hyper=info,info");
    }
    let mut configuration =
        InternalAuthustConfiguration::load().expect("Failed to load configuration");
    let password = std::mem::replace(&mut configuration.postgres.password, String::new());

    let config = ExportConfig::default();
    let resource = Resource::from_detectors(
        Duration::from_secs(0),
        vec![
            Box::new(ServiceNameDetector),
            Box::new(EnvResourceDetector::default()),
        ],
    );
    let tracer = otlp::new_pipeline()
        .tracing()
        .with_exporter(setup_span_exporter(&config))
        .with_trace_config(opentelemetry::sdk::trace::config().with_resource(resource))
        .install_batch(opentelemetry::runtime::Tokio)
        .expect("Failed to install opentelemetry tracer");
    let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let layer = tracing_subscriber::fmt::Layer::new().with_filter(EnvFilter::from_default_env());
    let registry = tracing_subscriber::registry()
        .with(EnvFilter::new(
            "tokio=trace,runtime=trace,trace,hyper=info,h2=info",
        ))
        .with(opentelemetry)
        .with(ErrorLayer::default())
        .with(layer);
    tracing::subscriber::set_global_default(registry).unwrap();
    LogTracer::init().unwrap();
    info!("Setting up database...");
    let shutdown_future = signal::ctrl_c().map(|fut| {
        fut.expect("Error occurred while waiting for Ctrl+C signal");
        tracing::info!("Received shutdown signal");
    });
    let pool = setup_database(&configuration.postgres, &password).await;
    let app_future = start_server(configuration, pool, shutdown_future);
    app_future.await;
    tracing::info!("Shutting down opentelemetry provider");
    opentelemetry::global::shutdown_tracer_provider();
    tracing::info!("Shutdown complete");
}

async fn setup_database(configuration: &PostgresConfiguration, password: &str) -> Pool {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&configuration.host)
        .port(configuration.port)
        .dbname(&configuration.database)
        .user(&configuration.user)
        .password(&password)
        .application_name("Authust");
    let mgr_config = ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Fast,
    };
    let manager = Manager::from_config(cfg, tokio_postgres::NoTls, mgr_config);
    let pool = Pool::builder(manager).max_size(16).build().unwrap();
    let mut client = pool.get().await.expect("Failed to get connection!");
    embedded::migrations::runner()
        .run_async(client.as_mut().deref_mut())
        .await
        .expect("Failed to run migrations");
    info!("Running migrations on database...");
    info!("Database setup complete");
    pool
}

#[derive(Debug, Clone)]
pub struct AuthustState {
    pub configuration: AuthustConfiguration,
}

#[derive(Clone)]
pub struct AppState {
    auth_data: AuthServiceData,
}

impl FromRef<AppState> for AuthServiceData {
    fn from_ref(input: &AppState) -> Self {
        input.auth_data.clone()
    }
}

async fn preload(storage: &StorageManager) -> Result<(), Arc<StorageError>> {
    info!("Preloading flows...");
    storage
        .get_for_data::<Flow>()
        .expect("Failed to get storage")
        .find_all(None)
        .await?;
    info!("Preloading stages...");
    storage
        .get_for_data::<Stage>()
        .expect("Failed to get storage")
        .find_all(None)
        .await?;
    info!("Preloading policies...");
    storage
        .get_for_data::<Policy>()
        .expect("Failed to get storage")
        .find_all(None)
        .await?;
    info!("Preloading prompts...");
    storage
        .get_for_data::<Prompt>()
        .expect("Failed to get storage")
        .find_all(None)
        .await?;
    info!("Preloading tenants...");
    storage
        .get_for_data::<Tenant>()
        .expect("Failed to get storage")
        .find_all(None)
        .await?;

    info!("Preload complete");
    Ok(())
}

#[derive(Clone)]
pub struct SharedState(Arc<InternalSharedState>);

impl SharedState {
    pub fn users(&self) -> &UserService {
        &self.0.users
    }

    pub fn executor(&self) -> &FlowExecutor {
        &self.0.executor
    }

    pub fn auth_data(&self) -> &AuthServiceData {
        &self.0.auth_data
    }

    pub fn storage(&self) -> &StorageManager {
        &self.0.storage
    }
    pub fn defaults(&self) -> &Defaults {
        &self.0.defaults.as_ref()
    }
    pub fn policies(&self) -> &PolicyService {
        &self.0.policies
    }
}

struct InternalSharedState {
    users: UserService,
    executor: FlowExecutor,
    auth_data: AuthServiceData,
    storage: StorageManager,
    defaults: Arc<Defaults>,
    policies: PolicyService,
}

pub struct Defaults {
    storage: StorageManager,
    pool: Pool,
    tenant: Option<Data<Tenant>>,
}

impl Defaults {
    pub fn tenant(&self) -> Option<Data<Tenant>> {
        self.tenant.clone()
    }
    pub fn pool(&self) -> Pool {
        self.pool.clone()
    }
    pub async fn connection(&self) -> Result<Object, PoolError> {
        self.pool.get().await
    }
}

impl Defaults {
    pub async fn new(storage: StorageManager, pool: Pool) -> Self {
        let default = Self::find_default(
            &storage,
            &pool.get().await.expect("Failed to get connection"),
        )
        .await;
        Defaults {
            storage,
            pool,
            tenant: default,
        }
    }

    async fn find_default(
        storage: &StorageManager,
        client: &impl GenericClient,
    ) -> Option<Data<Tenant>> {
        let statement = client
            .prepare_cached("select uid from tenants where is_default = true")
            .await
            .ok()
            .expect("Failed to prepare statement");
        let Some(row) = client
            .query_opt(&statement, &[])
            .await
            .expect("Failed to execute statement") else { return None };
        let tenant = storage
            .get_for_data::<Tenant>()
            .expect("Failed to get Tenant storage")
            .find_optional(&TenantQuery::uid(row.get("uid")))
            .await
            .expect("An error occurred while loading tenant");
        tenant
    }
}

async fn start_server(
    config: InternalAuthustConfiguration,
    pool: Pool,
    future: impl Future<Output = ()>,
) {
    #[cfg(debug_assertions)]
    #[cfg(feature = "dev-mode")]
    let cors = tower_http::cors::CorsLayer::very_permissive();
    let storage = storage::create_manager(pool.clone());
    preload(&storage).await.expect("Preloading failed");
    let policies = PolicyService::new(storage.clone(), pool.clone());
    let defaults = Defaults::new(storage.clone(), pool.clone()).await;
    let executor = FlowExecutor::new(storage.clone(), policies.clone());
    let users = UserService::new();
    let internal_state = InternalSharedState {
        users,
        executor,
        auth_data: AuthServiceData {
            encoding_key: EncodingKey::from_secret(config.secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(config.secret.as_bytes()),
        },
        storage: storage.clone(),
        defaults: Arc::new(defaults),
        policies,
    };
    let state = SharedState(Arc::new(internal_state));
    let service = ServiceBuilder::new()
        .layer(ExtensionLayer)
        .layer(otel_layer())
        .layer(CookieManagerLayer::new())
        .layer(HandleErrorLayer::new(handle_timeout_error))
        .timeout(Duration::from_secs(1));
    #[cfg(debug_assertions)]
    #[cfg(feature = "dev-mode")]
    let service = service.layer(cors);
    let router = Router::new()
        .route("/test", get(hello_world))
        .nest("/api/v1", setup_api_v1(&config.secret, state.clone()).await)
        .nest("/", setup_interface_router())
        .layer(service)
        .with_state(state);
    let bind = axum::Server::bind(&config.listen.http);
    info!("Listening on {}...", config.listen.http);
    bind.serve(router.into_make_service())
        .with_graceful_shutdown(future)
        .await
        .expect("Server crashed");
    // server.await.expect("Server crashed");
}

async fn handle_timeout_error(err: BoxError) -> (StatusCode, String) {
    if err.is::<tower::timeout::error::Elapsed>() {
        (
            StatusCode::REQUEST_TIMEOUT,
            "Request took too long".to_string(),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unhandled internal error: {}", err),
        )
    }
}

async fn hello_world() -> &'static str {
    "Hello, World!"
}
