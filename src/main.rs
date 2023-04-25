use bytes::Bytes;
use dotenv::dotenv;
use http::Uri;
use hyper::service::{make_service_fn};
use hyper::{Body, Client, Request, Response, Server};
use reqwest::Client as ReqwestClient;
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time;
use hyper::body::to_bytes;

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

impl From<Body> for ClonableBody {
    fn from(body: Body) -> Self {
        let body_bytes = tokio::runtime::Handle::current()
            .block_on(async { to_bytes(body).await })
            .unwrap();
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
    let client: ReqwestClient = ReqwestClient::new();
    let mut interval: time::Interval = time::interval(Duration::from_secs(10));

    loop {
        interval.tick().await;

        let rpc_servers_cloned: Vec<(String, u64, u128)> = rpc_servers.read().await.clone();
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
        *rpc_servers.write().await = updated_servers;
    }
}

async fn get_best_rpc(rpc_servers: Vec<RpcServer>) -> Option<RpcServer> {
    rpc_servers.first().cloned()
}

async fn proxy_request(
    clonable_req: &ClonableRequest,
    rpc_servers: Arc<RwLock<Vec<RpcServer>>>,
) -> Result<Response<Body>, ProxyError> {
    let client: Client<hyper::client::HttpConnector> = Client::new();

    for _ in 0..rpc_servers.read().await.len() {
        if let Some((url, _, _)) = get_best_rpc(rpc_servers.read().await.clone()).await {
            let uri: String = format!("http://{}{}", url, clonable_req.req.uri());
            let uri: Uri = uri.parse::<Uri>().unwrap();

            // Clone the request and create a new ClonableBody instance
            let cloned_body = ClonableBody(Arc::clone(&clonable_req.req.body().0));
            let cloned_req = Request::builder()
                .method(clonable_req.req.method().clone())
                .uri(uri)
                .version(clonable_req.req.version())
                .header(http::header::HOST, url.as_str())
                .body(cloned_body.into())
                .unwrap();

            match client.request(cloned_req).await {
                Ok(resp) => return Ok(resp),
                Err(_) => continue,
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
    let addr = ([127, 0, 0, 1], env::var("PORT")?.parse::<u16>()?).into();
    let server_urls: Vec<String> = env::var("RPC_SERVERS")?
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let rpc_servers: Arc<RwLock<Vec<(String, u64, u128)>>> = Arc::new(RwLock::new(Vec::new()));
    for url in server_urls {
        rpc_servers.write().await.push((url, 0, 0)); // Remove unwrap() call
    }

    tokio::spawn(update_rpc_stats(rpc_servers.clone()));

    let make_svc = make_service_fn(move |_conn| {
        let rpc_servers = rpc_servers.clone();
        async move {
            let cloned_req = ClonableRequest(req.clone());
            match proxy_request(&cloned_req, rpc_servers.clone()).await {
                Ok(response) => Ok(response),
                Err(_) => Ok(Response::builder().status(500).body(Body::empty()).unwrap()),
            }
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}
