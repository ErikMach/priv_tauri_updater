//! `PrivUpdater` creates a local server (reverse-proxy) for the latest release of a private github repo
//! so that a Tauri app can happily get the update.
//!
//! This requires a few changes but it turns out great...
#![warn(missing_docs)]

mod test;

use reqwest::{
    header::{
	HeaderMap,
	HeaderName,
	HeaderValue,
	ACCEPT,
	AUTHORIZATION,
	USER_AGENT,
    },
    Client,
};
use serde::Deserialize;
use std::{
    error::Error,
    collections::HashMap,
    net::{ IpAddr, Ipv4Addr, SocketAddr },
    sync::atomic::{ AtomicU8, Ordering },
};
use warp::{
    Reply,
    Filter, 
    reject::Reject as WarpReject,
};
use tokio::{
    sync::oneshot,
    net::TcpListener,
};

#[derive(Debug)]
#[allow(dead_code)]
struct ReqwestError(reqwest::Error);

impl WarpReject for ReqwestError {}

/// The shutdown signal for a server
///
/// Returned from creating a server with `PrivUpdater::new` or `serve`
#[derive(Debug)]
pub struct ShutdownSignal(oneshot::Sender<()>);

impl ShutdownSignal {
    /// Shutdown the server
    ///
    /// Note: this frees up the server address port that the server was running on
    pub fn shutdown(self) {
	let _ = self.0.send(());
    }
}

#[derive(Deserialize)]
struct GitHubAssetsList {
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name:			String,
    url:			String,
    browser_download_url:	String,
}

/// Holds all the necessary info to serve a reverse-proxy to your private github repo
#[derive(Debug)]
pub struct PrivUpdater {
    server_addr:	SocketAddr,
    client:		reqwest::Client,
    assets:		HashMap<String, String>,
    download_url_base:	String,
}

impl PrivUpdater {
    /// Constructs a new `PrivUpdater`
    ///
    /// # Arguments
    ///
    /// `gh_account_name` is case-insensitive
    /// 
    /// # Errors
    ///
    /// This function fails if `gh_account_name`, `gh_repo_name`, or `gh_token` are incorrect for GitHub
    /// or invalid as HeaderNames (see [reqwest docs](https://docs.rs/reqwest/latest/reqwest/header/struct.HeaderValue.html#method.from_str)).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // This example is not run since it uses dummy data to create the server, which will return an error
    /// # use tokio;
    /// use priv_tauri_updater::PrivUpdater;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let updater = PrivUpdater::new(
    ///         "MyGitHubAccount",
    ///         "MyGitHubRepo",
    ///         "MyGitHubToken",
    ///         Some(([127, 0, 0, 1], 8080))
    ///     ).await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn new<D, S>(gh_account_name: D, gh_repo_name: D, gh_token: D, server_addr: Option<S>) -> Result<Self, Box<dyn Error>>
    where
	D: std::fmt::Display,
	S: Into<SocketAddr> + 'static
    {
	let latest_release_url: String = format!("https://api.github.com/repos/{gh_account_name}/{gh_repo_name}/releases/latest");

	let mut headers = HeaderMap::new();
	let mut auth_value = HeaderValue::from_str( &format!("Bearer {gh_token}") )?;
	auth_value.set_sensitive(true);
	headers.insert(AUTHORIZATION, auth_value);
	headers.insert(HeaderName::from_static("x-github-api-version"), HeaderValue::from_static( "2022-11-28" ) );
	headers.insert(USER_AGENT,  HeaderValue::from_str( &format!("{gh_repo_name}") )?);

	let release_info = Client::new().get(latest_release_url)
	    .headers(headers.clone())
	    .header(ACCEPT, "application/vnd.github+json")
	    .send()
	    .await?
	    .json::<GitHubAssetsList>()
	    .await?;

	let download_url_base = release_info.assets[0].browser_download_url.rsplit_once('/').unwrap_or(("", "")).0.to_string();

	let assets = HashMap::<String, String>::from_iter(
	   release_info
		.assets
		.into_iter()
		.map(|file_info: GitHubAsset| (file_info.name, file_info.url))
	);

	headers.insert(ACCEPT, HeaderValue::from_static( "application/octet-stream" ));
	let client = Client::builder()
	    .default_headers(headers)
	    .build()?;

	Ok(Self {
	    server_addr: server_addr
		.map(|s| Into::<SocketAddr>::into(s))
		.unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 7748) ),
	    client,
	    assets,
	    download_url_base,
	})
    }
    /// Serve the update at the `server_addr` passed to `PrivUpdater::new()` (default: `127.0.0.1:7748`)
    pub async fn serve_update(mut self) -> Result<ShutdownSignal, Box<dyn Error>> {
	let (
	    assets,
	    client,
	    server_addr,
	    download_url_base,
	) = (
	    self.assets.clone(),
	    self.client.clone(),
	    String::from("http://") + &self.server_addr.to_string(),
	    self.download_url_base.clone(),
	);
	let routes = warp::path::param::<String>()
	    .and(warp::any().map(move || assets.clone() ))
	    .and(warp::any().map(move || client.clone() ))
	    .and(warp::any().map(move || server_addr.clone() ))
	    .and(warp::any().map(move || download_url_base.clone() ))
	    .and_then(move |
		filename:		String,
		assets:			HashMap<String, String>,
		client:			Client,
		server_addr:		String,
		download_url_base:	String,
	    | {	async move {
		let url: &String  = match assets.get(&filename) {
		    Some(value)	=> value,
		    None	=> return Err(warp::reject::not_found()),
		};
		if filename == "latest.json" {
		    get_latest_json(&client, url, &download_url_base, &server_addr.to_string())
			.await
			.map_err(|e| warp::reject::custom(ReqwestError(e)) )
		} else {
		    get_file(&client, url)
			.await
			.map_err(|e| warp::reject::custom(ReqwestError(e)) )
		}
	    }});

	let tx = self.serve_with_retry(routes).await?;

	Ok( ShutdownSignal(tx) )	
    }
    async fn serve_with_retry<F>(&mut self, routes: F) -> Result<oneshot::Sender<()>, String>
    where
	F: Filter + Clone + Send + Sync + 'static,
	F::Extract: Reply,
    {
	static COUNTER: AtomicU8 = AtomicU8::new(0);

	let (tx, rx) = oneshot::channel::<()>();

	if let Ok(listener) = TcpListener::bind(self.server_addr).await {
	    tokio::task::spawn(
		warp::serve(routes.clone())
		    .incoming(listener)
		    .graceful(async { rx.await.ok(); })
		    .run()
	    );
	    Ok(tx)
	} else if COUNTER.load(Ordering::Acquire) > 10 {
		Err(String::from("Unable to find unused port"))
	} else {
	    self.server_addr.set_port((self.server_addr.port() + 1) % 1000);
	    COUNTER.fetch_add(1, Ordering::Relaxed);
	    Box::pin(self.serve_with_retry(routes)).await
	}
    }
}

/// Convenience method to serve the update immediately at `http://127.0.0.1:7748`
///
/// # Examples
///
/// ```
/// // this example is ignored since tauri is a huge and unnecessary dev dependency
/// // in `src-tauri/src/lib.rs`
/// # use std::error::Error;
/// # use priv_tauri_updater::PrivUpdater;
/// # use tauri;
/// # use tauri_plugin_updater::UpdaterExt;
///
/// async fn update(app: tauri::AppHandle) -> Result<(), Box<dyn Error>> {
///     let update_server = priv_tauri_updater::serve("MyAccount", "MyRepo", "MyGitHubToken").await?;
///
///     if let Some(update) = app.updater()?.check().await? {
///
///         // ... your chosen download logic here
///
///     }
///
///     update_server.shutdown();
///
///     Ok(())
/// }
///
/// ```
///
/// # Errors
///
/// This function fails if:
///
/// - `gh_account_name`, `gh_repo_name`, or `gh_token` are incorrect for GitHub or invalid as HeaderNames (see [reqwest docs](https://docs.rs/reqwest/latest/reqwest/header/struct.HeaderValue.html#method.from_str))
/// - there are network errors (e.g. no internet connection)
/// - the server address `http://127.0.0.1:7748` is already in use
pub async fn serve<D: std::fmt::Display>(gh_account_name: D, gh_repo_name: D, gh_token: D) -> Result<ShutdownSignal, Box<dyn Error>> {
    let updater = PrivUpdater::new(gh_account_name, gh_repo_name, gh_token, None::<([u8; 4], u16)>).await?;
    let shutdown_signal = updater.serve_update().await?;
    Ok( shutdown_signal )
}


async fn get_latest_json(client: &Client, url: &str, download_url_base: &str, server_addr: &str) -> Result<Vec<u8>, reqwest::Error> {
    let text: String = client.get(url).send().await?.text().await?;
    Ok( text.replace(download_url_base, server_addr).into_bytes() )
}

async fn get_file(client: &Client, url: &str) -> Result<Vec<u8>, reqwest::Error> {
    Ok( client.get(url).send().await?.bytes().await?.to_vec() )
}