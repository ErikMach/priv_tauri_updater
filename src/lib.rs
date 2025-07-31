//! This lib is creates a local server (reverse-proxy) for the latest release of a private github repo
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
};
use warp::{
    Filter, 
    reject::Reject as WarpReject,
};
use tokio::sync::oneshot;

#[derive(Debug)]
#[allow(dead_code)]
struct ReqwestError(reqwest::Error);

impl WarpReject for ReqwestError {}

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

#[derive(Clone)]
struct ServerInfo {
    assets:		HashMap<String, String>,
    client:		Client,
    download_url_base:	String,
    server_addr:	String,
}

impl ServerInfo {
    fn from(priv_updater: &PrivUpdater) -> ServerInfo {
	ServerInfo {
	    assets:		priv_updater.assets.clone(),
	    client:		priv_updater.client.clone(),
	    download_url_base:	priv_updater.download_url_base.clone(),
	    server_addr:	priv_updater.server_addr.to_string(),
	}
    }
}

/// Holds all the necessary info to serve a reverse-proxy to your private github repo
pub struct PrivUpdater {
    server_addr:	SocketAddr,
    client:		reqwest::Client,
    assets:		HashMap<String, String>,
    download_url_base:	String,
    shutdown_signal:	Option<oneshot::Sender<()>>,
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
    /// # Examples
    ///
    /// ```rust
    /// let updater = PrivUpdater::new(
    ///     "MyGitHubAccount",
    ///     "MyGitHubRepo",
    ///     "MyGitHubToken",
    ///     ([127, 0, 0, 1], 8080)
    /// ).await?;
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
	    shutdown_signal: None,
	})
    }
    /// Serve the update at the `server_addr` passed to `PrivUpdater::new()` (default: `127.0.0.1:7748`)
    pub async fn serve_update(&mut self) -> Result<(), Box<dyn Error>> {
	let server_info: ServerInfo = ServerInfo::from(self);
	let routes = warp::path::param::<String>()
	    .and(warp::any().map(move || server_info.clone() ))
	    .and_then(move |
		filename: String,
		ServerInfo {
		    assets,
		    client,
		    server_addr,
		    download_url_base,
		}: ServerInfo
	    | {	async move {
		let url: &String  = match assets.get(&filename) {
		    Some(value)	=> value,
		    None	=> return Err(warp::reject::not_found()),
		};
		if filename == "latest.json" {
		    get_latest_json(&client, url, &download_url_base, &server_addr)
			.await
			.map_err(|e| warp::reject::custom(ReqwestError(e)) )
		} else {
		    get_file(&client, url)
			.await
			.map_err(|e| warp::reject::custom(ReqwestError(e)) )
		}
	    }});

	let (tx, rx) = oneshot::channel::<()>();

	let (_addr, server) = warp::serve(routes)
	    .bind_with_graceful_shutdown(self.server_addr, async {
	         rx.await.ok();
	    });

	tokio::task::spawn(server);

	self.shutdown_signal = Some(tx);

	Ok(())	
    }
    /// Shutdown the update server
    pub fn shutdown(&mut self) {
	if let Some(sender) = self.shutdown_signal.take() {
	    let _ = sender.send(());
	}
    }
}

async fn get_latest_json(client: &Client, url: &str, download_url_base: &str, server_addr: &str) -> Result<Vec<u8>, reqwest::Error> {
    let text: String = client.get(url).send().await?.text().await?;
    Ok( text.replace(download_url_base, server_addr).into_bytes() )
}

async fn get_file(client: &Client, url: &str) -> Result<Vec<u8>, reqwest::Error> {
    Ok( client.get(url).send().await?.bytes().await?.to_vec() )
}

/// Convenience method to serve the update immediately at `http://127.0.0.1:7748`
///
/// # Examples
///
/// ```rust
/// // in `src-tauri/src/lib.rs`
/// # use std::error::Error;
///
/// async fn update(app: tauri::AppHandle) -> Result<(), Box<dyn Error>> {
///     let update_server = priv_tauri_updater::serve("MyAccount", "MyRepo", "MyGitHubToken").await?;
///
///     if let Some(update) = app.updater()?.check().await? {
///
///	// ... your chosen download logic here
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
pub async fn serve<D: std::fmt::Display>(gh_account_name: D, gh_repo_name: D, gh_token: D) -> Result<PrivUpdater, Box<dyn Error>> {
    let mut updater = PrivUpdater::new(gh_account_name, gh_repo_name, gh_token, None::<([u8; 4], u16)>).await?;
    updater.serve_update().await?;
    Ok( updater )
}