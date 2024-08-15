use axum::{
    body::Body,
    extract::{Request, State},
    handler::Handler,
    http::uri::Uri,
    response::{IntoResponse, Response},
};
use clap::Parser;
use hyper::StatusCode;
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioExecutor};
use std::{borrow::Borrow, collections::VecDeque, ops::Deref, sync::Mutex, time::Duration};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

type Client = hyper_util::client::legacy::Client<HttpConnector, Body>;

type SharedState = Arc<RwLock<ServerConfig>>;

#[derive(Clone)]
struct ServerConfig {
    backends: Arc<Mutex<VecDeque<String>>>,
    client: Client,
}

type SharedBackendSet = Arc<Mutex<BackendSet>>;

#[derive(Clone, Debug)]
struct BackendSet {
    backends: VecDeque<String>,
    backend_health: HashMap<String, bool>,
}

// Idea:
// Have 2 data structures to hold the backend set data.
//  - backends: VecDeque<String>
//      rotating ring of backend server to use
//  - backend_health: HashMap<String, bool>
//      mapping of backend servers to their health state
//
// Have the `check_health` fn update the `backend_health` map, and have the
// handler read from this map when choosing a backend to use

#[tokio::main]
async fn main() {
    let args = Args::parse();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "load_balancer=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Populate the backend server list
    let (backends, backend_health) = build_backend_set(args);

    let backend_set = BackendSet {
        backends: backends.clone(),
        backend_health,
    };

    let backend_mutex = Arc::new(Mutex::new(backends));

    let client: Client =
        hyper_util::client::legacy::Client::<(), ()>::builder(TokioExecutor::new())
            .build(HttpConnector::new());

    let server_config = ServerConfig {
        backends: backend_mutex,
        client,
    };

    // tokio::spawn(check_health(backends_total, args.duration));

    let app = handle_request.with_state(server_config.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:4000")
        .await
        .unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

fn build_backend_set(args: Args) -> (VecDeque<String>, HashMap<String, bool>) {
    // Populate the backend server list
    let backend_servers = args.backends.split(';').collect::<Vec<&str>>();

    let mut backends: VecDeque<String> = VecDeque::with_capacity(backend_servers.len());
    // save to add any backends that failed & are healthy again
    let mut backends_total: HashSet<String> = HashSet::new();
    let mut backend_health: HashMap<String, bool> = HashMap::new();

    for bs in backend_servers {
        backends.push_back(bs.to_string());
        backends_total.insert(bs.to_string());
        backend_health.insert(bs.to_string(), true);
    }

    (backends, backend_health)
}

async fn handle_request(
    State(config): State<ServerConfig>,
    mut req: Request,
) -> Result<Response, StatusCode> {
    let path = req.uri().path();
    let path_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(path);

    tracing::debug!("got path {}", path);

    let uri = {
        let mut backends = config.backends.lock().unwrap();
        let backend_uri = backends.front().unwrap();
        let uri = format!("{}{}", backend_uri, path_query);
        tracing::debug!("routing to {}", uri);
        backends.rotate_left(1);
        uri
    };

    *req.uri_mut() = Uri::try_from(uri).unwrap();

    Ok(config
        .client
        .request(req)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .into_response())
}

async fn check_health(_backends_total: HashSet<String>, dur: u64) {
    tracing::debug!("duration: {}", dur);
    tokio::time::sleep(Duration::from_secs(dur)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(dur));
    loop {
        interval.tick().await;
        {}
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// frequency of health checks
    #[arg(short = 'd', long = "duration", default_value_t = 5 * 60)]
    duration: u64,

    /// list of backends (; separated)
    #[arg(short = 'b', long = "backends")]
    backends: String,
}
