#![allow(dead_code)]

use crate::{Error, Result};
use std::path::Path;
use std::process::Command;
use tokio_trace::{info, info_span};
use unthwart::unthwarted;

/// Ensure that rls analysis data is available and up to date.
pub async fn check(path: &Path) -> Result<()> {
    spoor::trace(info_span!("check"), async {
        info!("ensuring save-analysis data is available");

        info!("$ cd {} && cargo check", path.display());
        let path_ = path.to_owned();
        let status = unthwarted! {
            Command::new("cargo")
                .args(&["check"])
                .current_dir(path_)
                .status()?
        };
        if !status.success() {
            Err(Error::CargoCheckFailed)
        } else {
            Ok(())
        }
    })
    .await
}