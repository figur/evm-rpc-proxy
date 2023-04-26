use bytes::Bytes;
use dotenv::dotenv;
use hyper::body::to_bytes;
use hyper::service::make_service_fn;
use hyper::{Body, Request, Response, Server};
use reqwest;
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time;

#[derive(Debug)]
pub enum ProxyError {
    AllRpcServersFailed,
    HyperError(hyper::Error),
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProxyError::AllRpcServersFailed => write!(f, "All RPC servers failed"),
            ProxyError::HyperError(err) => write!(f, "Hyper error: {}", err),
        }
    }
}

impl std::error::Error for ProxyError {}

impl From<hyper::Error> for ProxyError {
    fn from(err: hyper::Error) -> ProxyError {
        ProxyError::HyperError(err)
    }
}
pub struct ClonableRequest {
    pub req: Request<ClonableBody>,
}

pub struct ClonableBody(pub Arc<Bytes>);

impl ClonableBody {
    pub async fn from_async(body: &mut Body) -> Self {
        let body_bytes = to_bytes(body).await.unwrap();
        ClonableBody(Arc::new(body_bytes))
    }
}

impl Clone for ClonableBody {
    fn clone(&self) -> Self {
        ClonableBody(self.0.clone())
    }
}

impl Into<Body> for ClonableBody {
    fn into(self) -> Body {
        Body::from(self.0.as_ref().clone())
    }
}

impl Clone for ClonableRequest {
    fn clone(&self) -> Self {
        let req = &self.req;
        let mut req_builder = Request::builder()
            .method(req.method().clone())
            .uri(req.uri().clone())
            .version(req.version());

        for (name, value) in req.headers().iter() {
            req_builder = req_builder.header(name, value);
        }

        let body = self.req.body().clone();
        req_builder.body(body).unwrap().into()
    }
}

impl From<Request<ClonableBody>> for ClonableRequest {
    fn from(req: Request<ClonableBody>) -> Self {
        ClonableRequest { req }
    }
}

type RpcServer = (String, u64, u128);

async fn update_rpc_stats(
    rpc_servers: Arc<RwLock<Vec<RpcServer>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client: reqwest::Client = reqwest::Client::new();
    let mut interval: time::Interval = time::interval(Duration::from_secs(10));

    loop {
        interval.tick().await;

        // println!("Updating RPC server stats");

        let rpc_servers_cloned: Vec<(String, u64, u128)> = rpc_servers.read().await.clone();
        let mut updated_servers: Vec<(String, u64, u128)> = Vec::new();
        for (url, _, _) in &rpc_servers_cloned {
            let start_time = std::time::Instant::now(); // Added: measure the start time
            let response_code: u128 = match client.get(url.clone()).send().await {
                Ok(resp) => resp.status().as_u16() as u128,
                Err(_) => continue,
            };

            // println!("Checking RPC server: {}", url); // Added log message

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
                Ok(json) => {
                    // Replace `unwrap()` with `?` operator to handle the error
                    let block_str = json["result"]
                        .as_str()
                        .ok_or("Failed to convert result to string")?;
                    u64::from_str_radix(&block_str[2..], 16)
                        .map_err(|e| format!("Failed to parse block number: {}", e))?
                }
                Err(_) => continue,
            };

            let latency = start_time.elapsed().as_millis(); // Added: calculate latency

            updated_servers.push((url.clone(), latest_block, latency));

            // Log the result of the check
            println!(
                "RPC server: {}, response code: {}, latest block: {}, latency (ms): {}",
                url, response_code, latest_block, latency
            );
        }

        updated_servers.sort_by_key(|(_, block, latency)| (u64::MAX - block, *latency));
        *rpc_servers.write().await = updated_servers;
    }
}

async fn get_best_rpc(rpc_servers: Vec<RpcServer>) -> Option<RpcServer> {
    let best_rpc = rpc_servers.first().cloned();
    if let Some((url, _, _)) = &best_rpc {
        println!("Selected best RPC server: {}", url); // Added log message
    }
    best_rpc
}

async fn proxy_request(
    clonable_req: &ClonableRequest,
    rpc_servers: Arc<RwLock<Vec<RpcServer>>>,
) -> Result<Response<Body>, ProxyError> {
    let client = reqwest::Client::new();
    let mut rpc_servers_clone = rpc_servers.read().await.clone();

    while !rpc_servers_clone.is_empty() {
        if let Some((url, _, _)) = get_best_rpc(rpc_servers_clone.clone()).await {
            let uri: String = format!("{}{}", url, clonable_req.req.uri());

            let rpc_server_url = url.clone();
            let rpc_server_host = rpc_server_url
                .split("://")
                .nth(1)
                .unwrap_or(&rpc_server_url)
                .split("/")
                .next()
                .unwrap_or(&rpc_server_url);

            let method = clonable_req.req.method().clone();
            let mut req_builder = client.request(method, &uri);
            for (name, value) in clonable_req.req.headers().iter() {
                // println!("{}: {:?}", name.as_str(), value);
                if let Ok(value_str) = value.to_str() {
                    if let (Ok(reqwest_name), Ok(reqwest_value)) = (
                        reqwest::header::HeaderName::from_str(name.as_str()),
                        reqwest::header::HeaderValue::from_str(value_str),
                    ) {
                        req_builder = req_builder.header(reqwest_name, reqwest_value);
                    }
                }
            }

            req_builder = req_builder.header("Host", rpc_server_host);

            let body = clonable_req.req.body().0.as_ref().clone();

            let req_builder = req_builder.body(body);

            match req_builder.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let body = Body::from(resp.bytes().await.unwrap());
 
                    let mut response_builder = Response::builder().status(status);
                    for (name, value) in headers {
                        if let Some(name) = name {
                            response_builder = response_builder.header(name, value);
                        }
                    }
                    let response = response_builder.body(body).unwrap();

                    println!("Request proxied successfully to {}", url);
                    return Ok(response);
                }
                Err(_) => {
                    println!("Request failed to proxy to {}, trying next server", url);
                    rpc_servers_clone.retain(|(failed_url, _, _)| failed_url != &url);
                    continue;
                }
            };
        } else {
            return Err(ProxyError::AllRpcServersFailed);
        }
    }

    Err(ProxyError::AllRpcServersFailed)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let addr = (
        [127, 0, 0, 1],
        env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse::<u16>()?,
    )
        .into();

    let server_urls: Vec<String> = env::var("RPC_SERVERS")
        .unwrap_or_else(|_| "".to_string())
        .split(',')
        .filter(|s| !s.trim().is_empty()) // Filter out empty strings
        .map(|s| s.trim().to_string())
        .collect();

    let rpc_servers: Arc<RwLock<Vec<(String, u64, u128)>>> = Arc::new(RwLock::new(Vec::new()));
    for url in server_urls {
        rpc_servers.write().await.push((url, 0, 0));
    }

    tokio::spawn(update_rpc_stats(rpc_servers.clone()));

    let make_svc = make_service_fn(move |_conn| {
        let rpc_servers = rpc_servers.clone();
        async {
            Ok::<_, hyper::Error>(hyper::service::service_fn(move |mut req: Request<Body>| {
                let rpc_servers = rpc_servers.clone();
                async move {
                    let clonable_body = ClonableBody::from_async(req.body_mut()).await;
                    let req = req.map(|_| clonable_body);
                    let cloned_req = ClonableRequest { req };
                    match proxy_request(&cloned_req, rpc_servers.clone()).await {
                        Ok(response) => Ok::<Response<Body>, hyper::Error>(response),
                        Err(_) => Ok(Response::builder().status(500).body(Body::empty()).unwrap()),
                    }
                }
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}
