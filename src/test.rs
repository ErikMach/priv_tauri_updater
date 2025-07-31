#![cfg(test)]

use super::*;
// use tokio::process::Command;
// use reqwest::Client;

#[tokio::test]
async fn it_works() {
    assert!(
	PrivUpdater::new(
	    "ErikMach",
	    "priv_tauri_updater",
	    "invalid_ghp",
	    None::<([u8; 4], u16)>
        ).await.is_err()
    );
}