use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::RequestBuilder;
use serde_json::{json, to_string};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use serde::de::{self};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load environment variables from .env file
    dotenv::dotenv().ok();
    let rpc_servers =
        env::var("RPC_SERVERS").unwrap_or_else(|_| "http://localhost:8545".to_string());
    let mut rpc_server_list = rpc_servers
        .split(',')
        .map(String::from)
        .collect::<Vec<String>>();
    let refresh_interval = env::var("REFRESH_INTERVAL")
        .unwrap_or_else(|_| "30".to_string())
        .parse::<u64>()
        .unwrap_or(30);

    // Set up header for JSON-RPC requests
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));

    // Start the proxy server on port 8080
    let addr = SocketAddr::from_str("0.0.0.0:8080")?;
    let listener = TcpListener::bind(addr)?;

    // Create shared state for RPC server info
    let shared_state = Arc::new(Mutex::new(ServerState::new(rpc_server_list.clone())));

    // Spawn a thread to periodically update the RPC server info
    let shared_state_ref = shared_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(refresh_interval)).await;

            let mut new_server_list = Vec::new();
            for rpc_server in rpc_server_list.iter() {
                let json_request = json!({
                    "jsonrpc": "2.0",
                    "method": "eth_blockNumber",
                    "params": [],
                    "id": 1,
                });

                let resp = reqwest::Client::new()
                    .post(rpc_server)
                    .headers(headers.clone())
                    .body(json_request.to_string())
                    .send().await;

                if let Ok(mut resp) = resp {
                    let mut buffer = String::new();
                    resp.read_to_string(&mut buffer)?;
                    if let Ok(response) = serde_json::from_str::<JsonResponse>(&buffer) {
                       
                        if let Some(block_number) = response.result {
                            let response_time = Instant::now().duration_since(response.sent_time);
                            new_server_list.push(ServerInfo {
                                rpc_server: rpc_server.clone(),
                                block_number: block_number,
                                response_time: response_time.as_millis() as u64,
                            });
                        }
                    }
                }
            }
    
            new_server_list.sort_by_key(|server| (server.response_time, server.block_number));
            let mut state = shared_state_ref.lock().unwrap();
            state.rpc_servers = new_server_list
                .iter()
                .map(|s| s.rpc_server.clone())
                .collect();
            state.best_server = new_server_list.first().map(|s| s.rpc_server.clone());
        }
    });
    
    // Start handling incoming requests
    for stream in listener.incoming() {
        let shared_state_ref = shared_state.clone();
        thread::spawn(move || {
            if let Ok(mut stream) = stream {
                // Read the incoming request
                let mut buffer = [0; 1024];
                let mut request = String::new();
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => request.push_str(&String::from_utf8_lossy(&buffer[..n])),
                        Err(_) => break,
                    }
                }
    
                // Parse the incoming request and find the best RPC server to forward it to
                let rpc_servers = shared_state_ref.lock().unwrap().rpc_servers.clone();
                let best_server = shared_state_ref.lock().unwrap().best_server.clone();
                let rpc_server =
                    best_server.unwrap_or_else(|| rpc_servers.first().unwrap().clone());
    
                // Send the incoming request to the best RPC server
                let response = reqwest::Client::new()
                    .post(&rpc_server)
                    .headers(headers)
                    .body(request)
                    .send();
    
                // Output logs based on the result of the request
                match response {
                    Ok(resp) => {
                        println!("Received request, forwarded to RPC server: {}", rpc_server);
                        if resp.status().is_client_error() || resp.status().is_server_error() {
                            println!(
                                "\x1b[31mRequest to RPC server {} failed: {}\x1b[0m",
                                rpc_server,
                                resp.status()
                            );
                        }
                        let mut stream = stream.try_clone().unwrap();
                        if let Err(_) = resp.copy_to(&mut stream) {}
                    }
                    Err(e) => {
                        println!(
                            "\x1b[31mRequest to RPC server {} failed: {}\x1b[0m",
                            rpc_server, e
                        );
                    }
                }
            }
        });
    }
    
    Ok(())
}

struct ServerInfo {
rpc_server: String,
block_number: String,
response_time: u64,
}

struct ServerState {
rpc_servers: Vec<String>,
best_server: Option<String>,
}

impl ServerState {
    fn new(rpc_servers: Vec<String>) -> Self {
        ServerState {
            rpc_servers: rpc_servers,
            best_server: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonResponse {
    jsonrpc: String,
    result: Option<String>,
    id: u64,
    sent_time: String,
}

impl<'de> Deserialize<'de> for JsonResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
        D: Deserializer<'de>,
        {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InnerJsonResponse {
        jsonrpc: String,
        result: Option<String>,
        id: u64,
        sent_time: String
    }
}

let inner = InnerJsonResponse::deserialize(deserializer)?;
    Ok(JsonResponse {
        jsonrpc: inner.jsonrpc,
        result: inner.result,
        id: inner.id,
        sent_time: Instant::from_str(&inner.sent_time).map_err(de::Error::custom)?,
    }
);

fn deserialize_in_place<D>(deserializer: D, place: &mut Self) -> Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        // Default implementation just delegates to `deserialize` impl.
        *place = try!(Deserialize::deserialize(deserializer));
        Ok(())
    }
}
