#![cfg(test)]

use super::*;
// use tokio::process::Command;
// use reqwest::Client;

#[tokio::test]
async fn updater_create_error() {
    assert!(
	PrivUpdater::new(
	    "ErikMach",
	    "priv_tauri_updater",
	    "invalid_ghp",
	    None::<([u8; 4], u16)>
        ).await.is_err()
    );
}