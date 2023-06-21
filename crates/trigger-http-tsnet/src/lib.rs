//! Implementation for the Spin HTTP engine.

mod spin;

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use anyhow::{Context, Error, Result};
use async_trait::async_trait;
use clap::Args;
use http::{uri::Scheme, StatusCode, Uri};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use serde::{Deserialize, Serialize};
use spin_app::{AppComponent, MetadataKey};
use spin_core::Engine;
use spin_http::{
    config::{HttpExecutorType, HttpTriggerConfig},
    routes::{RoutePattern, Router},
};
use spin_trigger::{
    locked::{BINDLE_VERSION_KEY, DESCRIPTION_KEY, VERSION_KEY},
    EitherInstancePre, TriggerAppEngine, TriggerExecutor,
};
use tsnet::{Network, ServerBuilder};
use tokio::net::TcpStream;
use tracing::log;

use crate::{spin::SpinHttpExecutor};

pub(crate) type RuntimeData = ();
pub(crate) type Store = spin_core::Store<RuntimeData>;

const TRIGGER_METADATA_KEY: MetadataKey<TriggerMetadata> = MetadataKey::new("trigger");

/// The Spin HTTP trigger.
pub struct HttpTrigger {
    engine: TriggerAppEngine<Self>,
    router: Router,
    // Base path for component routes.
    base: String,
    // Hostname for tailnet
    hostname: String,
    // Component ID -> component trigger config
    component_trigger_configs: HashMap<String, HttpTriggerConfig>,
}

#[derive(Args)]
pub struct CliArgs {
    /// IP address and port to listen on
    #[clap(long = "post", default_value = "3000")]
    pub port: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TriggerMetadata {
    r#type: String,
    base: String,
    hostname: String,
}

#[async_trait]
impl TriggerExecutor for HttpTrigger {
    const TRIGGER_TYPE: &'static str = "tailscale";
    type RuntimeData = RuntimeData;
    type TriggerConfig = HttpTriggerConfig;
    type RunConfig = CliArgs;

    async fn new(engine: TriggerAppEngine<Self>) -> Result<Self> {
        let base = engine.app().require_metadata(TRIGGER_METADATA_KEY)?.base;
        let hostname = engine.app().require_metadata(TRIGGER_METADATA_KEY)?.hostname;

        let component_routes = engine
            .trigger_configs()
            .map(|(_, config)| (config.component.as_str(), config.route.as_str()));

        let (router, duplicate_routes) = Router::build(&base, component_routes)?;

        if !duplicate_routes.is_empty() {
            log::error!("The following component routes are duplicates and will never be used:");
            for dup in &duplicate_routes {
                log::error!(
                    "  {}: {} (duplicate of {})",
                    dup.replaced_id,
                    dup.route.full_pattern_non_empty(),
                    dup.effective_id,
                );
            }
        }

        log::trace!(
            "Constructed router for application {}: {:?}",
            engine.app_name,
            router.routes().collect::<Vec<_>>()
        );

        let component_trigger_configs = engine
            .trigger_configs()
            .map(|(_, config)| (config.component.clone(), config.clone()))
            .collect();

        Ok(Self {
            engine,
            router,
            base,
            hostname,
            component_trigger_configs,
        })
    }

    async fn run(self, config: Self::RunConfig) -> Result<()> {
        let port = config.port;

        // Print startup messages
        let scheme = "http";
        let base_url = format!("{}://{:?}:{:?}", scheme, self.hostname, port);
        terminal::step!("\nServing", "{}", base_url);
        log::info!("Serving {}", base_url);

        println!("Available Routes:");
        for (route, component_id) in self.router.routes() {
            println!("  {}: {}{}", component_id, base_url, route);
            if let Some(component) = self.engine.app().get_component(component_id) {
                if let Some(description) = component.get_metadata(DESCRIPTION_KEY)? {
                    println!("    {}", description);
                }
            }
        }

        self.serve(port).await?;
        Ok(())
    }

    async fn instantiate_pre(
        engine: &Engine<Self::RuntimeData>,
        component: &AppComponent,
        config: &Self::TriggerConfig,
    ) -> Result<EitherInstancePre<Self::RuntimeData>> {
        if let Some(HttpExecutorType::Wagi(_)) = &config.executor {
            let module = component.load_module(engine).await?;
            Ok(EitherInstancePre::Module(
                engine
                    .module_instantiate_pre(&module)
                    .map_err(spin_trigger::decode_preinstantiation_error)?,
            ))
        } else {
            let comp = component.load_component(engine).await?;
            Ok(EitherInstancePre::Component(
                engine
                    .instantiate_pre(&comp)
                    .map_err(spin_trigger::decode_preinstantiation_error)?,
            ))
        }
    }
}

impl HttpTrigger {
    /// Handles incoming requests using an HTTP executor.
    pub async fn handle(
        &self,
        mut req: Request<Body>,
        scheme: Scheme,
    ) -> Result<Response<Body>> {
        set_req_uri(&mut req, scheme)?;

        log::info!(
            "Processing request for application {} on URI {}",
            &self.engine.app_name,
            req.uri()
        );

        let path = req.uri().path();

        // Handle well-known spin paths
        if let Some(well_known) = path.strip_prefix(spin_http::WELL_KNOWN_PREFIX) {
            return match well_known {
                "health" => Ok(Response::new(Body::from("OK"))),
                "info" => self.app_info(),
                _ => Self::not_found(),
            };
        }

        // Route to app component
        match self.router.route(path) {
            Ok(component_id) => {
                let trigger = self.component_trigger_configs.get(component_id).unwrap();

                let executor = trigger.executor.as_ref().unwrap_or(&HttpExecutorType::Spin);

                let res = match executor {
                    HttpExecutorType::Spin => {
                        let executor = SpinHttpExecutor;
                        executor
                            .execute(
                                &self.engine,
                                component_id,
                                &self.base,
                                &trigger.route,
                                req,
                            )
                            .await
                    }
                    HttpExecutorType::Wagi(_wagi_config) => {
                        log::error!("Wagi executor is currently unsupported");
                        Self::internal_error(None)
                    }
                };
                match res {
                    Ok(res) => Ok(res),
                    Err(e) => {
                        log::error!("Error processing request: {:?}", e);
                        Self::internal_error(None)
                    }
                }
            }
            Err(_) => Self::not_found(),
        }
    }

    /// Returns spin status information.
    fn app_info(&self) -> Result<Response<Body>> {
        let info = AppInfo {
            name: self.engine.app_name.clone(),
            version: self.engine.app().get_metadata(VERSION_KEY)?,
            bindle_version: self.engine.app().get_metadata(BINDLE_VERSION_KEY)?,
        };
        let body = serde_json::to_vec_pretty(&info)?;
        Ok(Response::builder()
            .header("content-type", "application/json")
            .body(body.into())?)
    }

    /// Creates an HTTP 500 response.
    fn internal_error(body: Option<&str>) -> Result<Response<Body>> {
        let body = match body {
            Some(body) => Body::from(body.as_bytes().to_vec()),
            None => Body::empty(),
        };

        Ok(Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(body)?)
    }

    /// Creates an HTTP 404 response.
    fn not_found() -> Result<Response<Body>> {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())?)
    }

    async fn serve(self, port: u16) -> Result<()> {
        let self_ = Arc::new(self);
        let make_service = make_service_fn(|_conn: &TcpStream| {
            let self_ = self_.clone();
            async move {
                let service = service_fn(move |req| {
                    let self_ = self_.clone();
                    async move { self_.handle(req, Scheme::HTTP).await }
                });
                Ok::<_, Error>(service)
            }
        });

        let ts = ServerBuilder::new()
            .ephemeral()
            .redirect_log()
            .hostname(self_.hostname.as_str())
            .build()
            .unwrap();
        let listen_addr = format!(":{:?}", port);
        let listener = ts.listen_async(Network::Tcp, listen_addr.as_str()).unwrap();
        Server::builder(listener).serve(make_service).await?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindle_version: Option<String>,
}

fn parse_listen_addr(addr: &str) -> anyhow::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    // Prefer 127.0.0.1 over e.g. [::1] because CHANGE IS HARD
    if let Some(addr) = addrs
        .iter()
        .find(|addr| addr.is_ipv4() && addr.ip() == Ipv4Addr::LOCALHOST)
    {
        return Ok(*addr);
    }
    // Otherwise, take the first addr (OS preference)
    addrs.into_iter().next().context("couldn't resolve address")
}

fn set_req_uri(req: &mut Request<Body>, scheme: Scheme) -> Result<()> {
    const DEFAULT_HOST: &str = "localhost";

    let authority_hdr = req
        .headers()
        .get(http::header::HOST)
        .map(|h| h.to_str().context("Expected UTF8 header value (authority)"))
        .unwrap_or(Ok(DEFAULT_HOST))?;
    let uri = req.uri().clone();
    let mut parts = uri.into_parts();
    parts.authority = authority_hdr
        .parse()
        .map(Option::Some)
        .map_err(|e| anyhow::anyhow!("Invalid authority {:?}", e))?;
    parts.scheme = Some(scheme);
    *req.uri_mut() = Uri::from_parts(parts).unwrap();
    Ok(())
}

// We need to make the following pieces of information available to both executors.
// While the values we set are identical, the way they are passed to the
// modules is going to be different, so each executor must must use the info
// in its standardized way (environment variables for the Wagi executor, and custom headers
// for the Spin HTTP executor).
const FULL_URL: &[&str] = &["SPIN_FULL_URL", "X_FULL_URL"];
const PATH_INFO: &[&str] = &["SPIN_PATH_INFO", "PATH_INFO"];
const MATCHED_ROUTE: &[&str] = &["SPIN_MATCHED_ROUTE", "X_MATCHED_ROUTE"];
const COMPONENT_ROUTE: &[&str] = &["SPIN_COMPONENT_ROUTE", "X_COMPONENT_ROUTE"];
const RAW_COMPONENT_ROUTE: &[&str] = &["SPIN_RAW_COMPONENT_ROUTE", "X_RAW_COMPONENT_ROUTE"];
const BASE_PATH: &[&str] = &["SPIN_BASE_PATH", "X_BASE_PATH"];
const CLIENT_ADDR: &[&str] = &["SPIN_CLIENT_ADDR", "X_CLIENT_ADDR"];

pub(crate) fn compute_default_headers<'a>(
    uri: &Uri,
    raw: &str,
    base: &str,
    host: &str,
    client_addr: SocketAddr,
) -> Result<Vec<(&'a [&'a str], String)>> {
    let mut res = vec![];
    let abs_path = uri
        .path_and_query()
        .expect("cannot get path and query")
        .as_str();

    let path_info = RoutePattern::from(base, raw).relative(abs_path)?;

    let scheme = uri.scheme_str().unwrap_or("http");

    let full_url = format!("{}://{}{}", scheme, host, abs_path);
    let matched_route = RoutePattern::sanitize_with_base(base, raw);

    res.push((PATH_INFO, path_info));
    res.push((FULL_URL, full_url));
    res.push((MATCHED_ROUTE, matched_route));

    res.push((BASE_PATH, base.to_string()));
    res.push((RAW_COMPONENT_ROUTE, raw.to_string()));
    res.push((
        COMPONENT_ROUTE,
        raw.to_string()
            .strip_suffix("/...")
            .unwrap_or(raw)
            .to_string(),
    ));
    res.push((CLIENT_ADDR, client_addr.to_string()));

    Ok(res)
}

/// The HTTP executor trait.
/// All HTTP executors must implement this trait.
#[async_trait]
pub(crate) trait HttpExecutor: Clone + Send + Sync + 'static {
    // TODO: allowing this lint because I want to gather feedback before
    // investing time in reorganizing this
    async fn execute(
        &self,
        engine: &TriggerAppEngine<HttpTrigger>,
        component_id: &str,
        base: &str,
        raw_route: &str,
        req: Request<Body>,
    ) -> Result<Response<Body>>;
}

