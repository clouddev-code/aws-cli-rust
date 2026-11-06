//! awsr: an experimental Rust port of the data-driven core of the AWS CLI.

pub mod checksum;
pub mod cli;
pub mod client;
pub mod commands;
pub mod config;
pub mod context;
pub mod credentials;
pub mod customize;
pub mod driver;
pub mod endpoint;
pub mod http;
pub mod loader;
pub mod model;
pub mod naming;
pub mod oauth;
pub mod output;
pub mod paginate;
pub mod protocol;
pub mod sigv4;
pub mod timestamp;
pub mod util;
pub mod validate;
pub mod value;
