use anyhow::Result;
use clap::Parser;
use wecode::cli::{Cli, dispatch};

#[tokio::main]
async fn main() -> Result<()> {
    dispatch(Cli::parse()).await
}
