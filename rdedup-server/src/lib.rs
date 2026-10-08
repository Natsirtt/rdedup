pub mod config;
mod http;
pub mod lease;
pub use http::router;

pub async fn run(
    config_path: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    http::run(config_path).await
}
