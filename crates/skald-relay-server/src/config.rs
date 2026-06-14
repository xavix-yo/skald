//! Runtime configuration, read from the environment (relay.md §7). Sensible
//! defaults so the relay boots with zero config in local dev.
//!
//! | Env var      | Meaning                          | Default          |
//! |--------------|----------------------------------|------------------|
//! | `RELAY_BIND` | full `ip:port` to listen on      | `0.0.0.0:8080`   |
//! | `PORT`       | port only (used if no RELAY_BIND)| —                |
//! | `RELAY_DB`   | SQLite file path                 | `data/relay.db`  |

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: String,
}

impl Config {
    pub fn from_env() -> Config {
        let default_bind: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let bind = std::env::var("RELAY_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                std::env::var("PORT")
                    .ok()
                    .and_then(|p| format!("0.0.0.0:{p}").parse().ok())
            })
            .unwrap_or(default_bind);
        let db_path = std::env::var("RELAY_DB").unwrap_or_else(|_| "data/relay.db".into());
        Config { bind, db_path }
    }
}
