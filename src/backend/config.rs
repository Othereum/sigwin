//! The server's remote config, of which Sigwin uses one value: how large an attachment may be.
//! The official app reads its limit from here too; the server has let it grow past the 100MB
//! everyone assumed.

use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::manager::RegistrationData;

use super::*;

/// The official app asks at most this often.
const REFRESH_MS: u64 = 2 * 3_600_000;
const MAX_BYTES: &str = "global.attachments.maxBytes";

pub(super) enum Config {
    /// the ETag still matches
    Unchanged,
    /// `max_bytes` None: the server doesn't say, and the default applies
    New { etag: Option<String>, max_bytes: Option<u64> },
}

impl App {
    /// Reads the config in the background, unless it was read lately.
    pub(super) fn refresh_config(&mut self) {
        if self.config_fetching || view::time::now_ms().saturating_sub(self.persist.config_at) < REFRESH_MS {
            return;
        }
        let Some(m) = self.manager.as_ref() else { return };
        let reg = m.registration_data().clone();
        let etag = self.persist.config_etag.clone();
        let tx = self.tx.clone();
        self.config_fetching = true;
        tokio::task::spawn_local(async move {
            let res = fetch(&reg, etag.as_deref()).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::Config(res));
        });
    }

    pub(super) fn config_fetched(&mut self, res: Result<Config, String>) {
        self.config_fetching = false;
        match res {
            Ok(Config::Unchanged) => debug!("remote config unchanged"),
            Ok(Config::New { etag, max_bytes }) => {
                info!(max_bytes, "remote config");
                self.persist.config_etag = etag;
                self.persist.send_limit = max_bytes;
                (self.emit)(Event::SendLimit(max_bytes));
            }
            // tried again after the same wait: the limit known so far stays
            Err(e) => warn!("remote config: {e}"),
        }
        self.persist.config_at = view::time::now_ms();
        self.persist_dirty = true;
    }
}

async fn fetch(reg: &RegistrationData, etag: Option<&str>) -> anyhow::Result<Config> {
    let service = super::storage::push_service(reg)?;
    let mut req = service.request(reqwest::Method::GET, Endpoint::service("/v2/config"), HttpAuthOverride::NoOverride)?;
    if let Some(etag) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let res = req.send().await.map_err(reqwest::Error::without_url)?;
    if res.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Config::Unchanged);
    }
    let res = res.error_for_status().map_err(reqwest::Error::without_url)?;
    let etag = res.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(String::from);
    let body = res.bytes().await.map_err(reqwest::Error::without_url)?;
    Ok(Config::New { etag, max_bytes: max_bytes(&body)? })
}

/// The limit in a `v2/config` body: `{"config": {name: value}}`, every value a string.
fn max_bytes(body: &[u8]) -> anyhow::Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Body {
        config: HashMap<String, serde_json::Value>,
    }
    let body: Body = serde_json::from_slice(body)?;
    Ok(body.config.get(MAX_BYTES).and_then(|v| v.as_str()).and_then(|s| s.trim().parse().ok()).filter(|&n| n > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_read_from_the_config() {
        let body = br#"{"config": {"global.attachments.maxBytes": "209715200", "desktop.x": "true"}}"#;
        assert_eq!(max_bytes(body).unwrap(), Some(200 * 1024 * 1024));
        // not given, or not a size: the default applies
        assert_eq!(max_bytes(br#"{"config": {"desktop.x": "true"}}"#).unwrap(), None);
        assert_eq!(max_bytes(br#"{"config": {"global.attachments.maxBytes": "lots"}}"#).unwrap(), None);
        assert_eq!(max_bytes(br#"{"config": {"global.attachments.maxBytes": "0"}}"#).unwrap(), None);
        assert!(max_bytes(b"[]").is_err());
    }
}
