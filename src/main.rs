use http::Uri;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Request, Response, Server};
use reqwest::Client as ReqwestClient;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use dotenv::dotenv;
use std::env;

type RpcServer = (String, u64, u128);

async fn update_rpc_stats(rpc_servers: Arc<Mutex<Vec<RpcServer>>>) -> Result<(), Box<dyn std::error::Error>> {
    let client: ReqwestClient = ReqwestClient::new();
    let mut interval: time::Interval = time::interval(Duration::from_secs(10));

    loop {
        interval.tick().await;

        let rpc_servers_cloned: Vec<(String, u64, u128)> = rpc_servers.lock().unwrap().clone();
        let mut updated_servers: Vec<(String, u64, u128)> = Vec::new();
        for (url, _, _) in &rpc_servers_cloned {
            let latency: u128 = match client.get(url.clone()).send().await {
                Ok(resp) => resp.status().as_u16() as u128,
                Err(_) => continue,
            };

            let latest_block: u64 = match client
                .post(url.clone())
                .json(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_blockNumber",
                    "params": []
                }))
                .send()
                .await?
                .json::<Value>()
                .await
            {
                Ok(json) => json["result"].as_str().unwrap()[2..]
                    .parse::<u64>()
                    .unwrap(),
                Err(_) => continue,
            };

            updated_servers.push((url.clone(), latest_block, latency));
        }

        updated_servers.sort_by_key(|(_, block, latency)| (u64::MAX - block, *latency));
        *rpc_servers.lock().unwrap() = updated_servers;
    }
}

async fn proxy_request(
    req: Request<Body>,
    rpc_servers: Arc<Mutex<Vec<RpcServer>>>,
) -> Result<Response<Body>, hyper::Error> {
    let best_rpc: (String, u64, u128) = rpc_servers.lock().unwrap()[0].clone();
    let uri: String = format!("http://{}{}", best_rpc.0, req.uri());
    let uri: Uri = uri.parse::<Uri>().unwrap();
    let client: Client<hyper::client::HttpConnector> = Client::new();

    let mut builder: http::request::Builder = Request::builder()
        .method(req.method().clone())
        .uri(uri)
        .version(req.version());

    for (name, value) in req.headers().iter() {
        builder = builder.header(name, value);
    }

    let req: Request<Body> = builder.body(req.into_body()).unwrap();
    client.request(req).await
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    let rpc_servers: Vec<String> = env::var("RPC_SERVERS")
        .expect("RPC_SERVERS must be set in .env file")
        .split(',')
        .map(|s| s.to_owned())
        .collect();

    let rpc_servers_data: Vec<(String, u64, u128)> = rpc_servers
        .iter()
        .map(|url| (url.clone(), 0, u128::MAX))
        .collect();

    let rpc_servers: Arc<Mutex<Vec<(String, u64, u128)>>> = Arc::new(Mutex::new(rpc_servers_data));

    let rpc_servers_clone: Arc<Mutex<Vec<(String, u64, u128)>>> = rpc_servers.clone();

    tokio::spawn(async move {
        if let Err(e) = update_rpc_stats(rpc_servers_clone).await {
            eprintln!("Error updating RPC stats: {}", e);
        }
    });

    let make_svc = make_service_fn(move |_conn: &hyper::server::conn::AddrStream| {
        let rpc_servers: Arc<Mutex<Vec<(String, u64, u128)>>> = rpc_servers.clone();
        async {
            Ok::<_, hyper::Error>(service_fn(move |req: Request<Body>| {
                proxy_request(req, rpc_servers.clone())
            }))
        }
    });

    let addr: std::net::SocketAddr = ([127, 0, 0, 1], 8080).into();
    let server = Server::bind(&addr).serve(make_svc);

    println!("EVM RPC Proxy running on http://{}", addr);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

