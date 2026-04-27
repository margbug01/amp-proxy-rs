use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml parse: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("http build: {0}")]
    HttpBuild(#[from] http::Error),

    #[error("upstream: {0}")]
    Upstream(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, AppError>;
