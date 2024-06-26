use std::{net::SocketAddr, path::PathBuf};

use anyhow::Context;
use axum::{
    extract::{Request, State},
    http::HeaderMap,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post, put},
    Router,
};
use clap::Parser;
use job_hub::{
    cli_args::CliArgs,
    openapi::build_openapi,
    routes,
    server::{response::ApiError, state::ApiState},
};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
    decompression::RequestDecompressionLayer,
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable};
use utoipa_swagger_ui::SwaggerUi;

fn init_tracing() -> anyhow::Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt::Subscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .context("Failed to set global tracing subscriber")?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "job_hub=trace,tower_http=trace");
    }

    init_tracing()?;

    let cli_args = CliArgs::parse();

    let state = ApiState::new(cli_args.api_token, cli_args.projects_dir);

    let api = Router::new()
        .route(
            "/request_chat_id",
            get(routes::request_chat_id::request_chat_id),
        )
        .route("/cancel/:id", put(routes::cancel::cancel))
        .route("/status/:id", get(routes::status::status))
        .route("/list_log_files", get(routes::log_files::list_log_files))
        .route(
            "/download_zip_file",
            post(routes::download_zip_file::download_zip_file),
        )
        .route(
            "/get_log_file_text",
            get(routes::log_files::get_log_file_text),
        )
        .route(
            "/gs_log_to_locust_converter",
            post(routes::gs_log_to_locust_converter::gs_log_to_locust_converter),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            validate_bearer_token,
        ));

    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

    let server_urls = cli_args.server_urls;
    let openapi = build_openapi(server_urls);

    let app = Router::new()
        .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .nest("/api", api)
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi.clone()))
        .merge(Redoc::with_url("/redoc", openapi))
        .merge(RapiDoc::new("/api-docs/openapi.json").path("/rapidoc"))
        .layer(
            ServiceBuilder::new()
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(
                            DefaultMakeSpan::new()
                                .level(tracing::Level::INFO)
                                .include_headers(true),
                        )
                        .on_request(DefaultOnRequest::new().level(tracing::Level::INFO))
                        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
                )
                .layer(RequestDecompressionLayer::new())
                .layer(CompressionLayer::new())
                .layer(CorsLayer::permissive()),
        );

    let addr = cli_args.socket_address;

    tracing::info!(%addr, "Starting server");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("Bind failed")?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("Server failed")?;

    Ok(())
}

async fn validate_bearer_token(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, ApiError> {
    let api_key = headers
        .get("api_key")
        .ok_or_else(|| {
            tracing::warn!("api_key header not present");
            ApiError::ApiKeyMissing
        })?
        .to_str()
        .map_err(|_| {
            tracing::warn!("Failed to convert api_key header into str");
            ApiError::ApiKeyMissing
        })?;

    if !state.api_token_valid(api_key) {
        tracing::warn!(%api_key, "Invalid api_key");
        return Err(ApiError::ApiKeyInvalid);
    }

    let res = next.run(request).await;

    Ok(res)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutting down");
}
