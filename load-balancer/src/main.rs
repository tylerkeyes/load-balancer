use axum::{
    body::Body,
    extract::{Request, State},
    handler::Handler,
    http::uri::Uri,
    response::{IntoResponse, Response},
};
use clap::Parser;
use core::panic;
use hyper::StatusCode;
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioExecutor};
use serde::{Deserialize, Serialize};
use std::fs;
use std::{collections::VecDeque, sync::Mutex, time::Duration};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::watch;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

type Client = hyper_util::client::legacy::Client<HttpConnector, Body>;

type LbSender = watch::Sender<HashMap<String, bool>>;
type LbReceiver = watch::Receiver<HashMap<String, bool>>;

#[derive(Clone)]
struct ServerConfig {
    backends: Arc<Mutex<VecDeque<String>>>,
    client: Client,
    receiver: LbReceiver,
}

/// Command line arguments available to the load balancer:
///
/// duration: frequency of health checks being run
/// backends: ';' separated list of backend servers (https://x.x.x.x:80)
/// log_level: level for logging, from (info, debug)
/// config_file: configuration file alternative to command line arguments
#[derive(Clone, Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// frequency of health checks
    #[arg(short = 'd', long = "duration", default_value_t = 5 * 60)]
    duration: u64,

    /// list of backends (; separated)
    #[arg(short = 'b', long = "backends", default_value_t = String::from(""))]
    backends: String,

    /// logging level, from the options `info` & `debug`
    #[arg(short = 'l', long = "loglevel", default_value_t = String::from("info"))]
    log_level: String,

    /// configuration file containing command-line arguments
    #[arg(short = 'c', long = "config", default_value_t = String::from(""))]
    config_file: String,
}

/// TOML parsing struct, holds config details
#[derive(Serialize, Deserialize)]
struct ConfigArgs {
    duration: u64,
    backends: String,
    log_level: String,
}

/// TOML parsing struct, holds top-level details
#[derive(Deserialize, Serialize)]
struct Config {
    args: ConfigArgs,
}

#[tokio::main]
async fn main() {
    // parse env args & initialize logging client
    let mut args = Args::parse();
    if !args.config_file.is_empty() {
        let updated_args = parse_config_file(args.config_file.clone());
        args = updated_args;
    }

    if args.backends.is_empty() {
        panic!("'backends' cannot be empty");
    }

    set_logging_level(args.log_level.clone());

    // Populate the backend server list
    let (backends, backend_health) = build_backends(args.clone());
    let backend_mutex = Arc::new(Mutex::new(backends));

    let client: Client =
        hyper_util::client::legacy::Client::<(), ()>::builder(TokioExecutor::new())
            .build(HttpConnector::new());

    // initialize a channel to manage the current status of healthy backend servers.
    // sender updates the reciever with the latest healthy servers from the available ones.
    let (tx, rx) = watch::channel(backend_health.clone());

    let server_config = ServerConfig {
        backends: backend_mutex,
        client: client.clone(),
        receiver: rx,
    };

    // runs according to the frequency specified by the `duration` parameter, defaulting to 5
    // minutes.
    // this process will then send an updated map of the healthy backend servers to the handler.
    tokio::spawn(check_health(
        backend_health,
        tx,
        client.clone(),
        args.duration,
    ));

    let app = handle_request.with_state(server_config.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4000").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn handle_request(
    State(mut config): State<ServerConfig>,
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
        // get the possible backends and pull the top
        let mut backends = config.backends.lock().unwrap();
        let mut backend_uri = backends.front().unwrap();

        let health_map = config.receiver.borrow_and_update();
        tracing::debug!("read health map: {:?}", health_map);

        let mut idx = 0;
        while !health_map.get(backend_uri).unwrap() && idx < backends.len() {
            backends.rotate_left(1);
            backend_uri = backends.front().unwrap();
            idx += 1;
        }

        let uri = format!("{}{}", backend_uri, path_query);
        tracing::debug!("routing to {}", uri);

        // move the first backend server to the end
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

async fn check_health(
    backends_health: HashMap<String, bool>,
    sender: LbSender,
    client: Client,
    dur: u64,
) {
    tracing::debug!("duration: {}", dur);
    tokio::time::sleep(Duration::from_secs(dur)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(dur));
    let mut backends = backends_health.clone();
    loop {
        interval.tick().await;
        tracing::info!("backend health map: {:?}", backends);

        for (server, _) in backends.clone() {
            let uri = Uri::try_from(server.clone()).unwrap();
            let resp = client.get(uri).await;

            let mut is_healthy = resp.is_ok();
            if is_healthy {
                let val = resp.unwrap().into_response();
                let status_code = val.status();
                if status_code.is_server_error() {
                    is_healthy = false;
                }
            }

            backends.insert(server.clone(), is_healthy);
        }

        // send the updated map to the primary handler
        let resp = sender.send(backends.clone());
        match resp {
            Ok(_) => tracing::info!("updated message"),
            Err(_) => tracing::info!("error when sending message"),
        };
    }
}

fn build_backends(args: Args) -> (VecDeque<String>, HashMap<String, bool>) {
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

fn set_logging_level(log_level: String) {
    if log_level != "debug" && log_level != "info" {
        panic!("invalid log level provided");
    }
    let logging_str = format!("load_balancer={}", log_level);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| logging_str.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn parse_config_file(file_name: String) -> Args {
    let config: Config = {
        let config_text = fs::read_to_string(file_name.clone()).expect("error reading file");
        toml::from_str(&config_text).expect("error reading stream")
    };

    let _serialized = serde_json::to_string(&config).expect("error serializing toml");

    Args {
        duration: config.args.duration,
        backends: config.args.backends,
        log_level: config.args.log_level,
        config_file: file_name.clone(),
    }
}
