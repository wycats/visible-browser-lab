use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEndpoint {
    origin: Url,
}

impl CdpEndpoint {
    pub fn parse(endpoint: &str) -> anyhow::Result<Self> {
        Ok(Self {
            origin: Url::parse(endpoint)?,
        })
    }

    pub fn version_url(&self) -> Url {
        self.join("/json/version")
    }

    pub fn targets_url(&self) -> Url {
        self.join("/json/list")
    }

    fn join(&self, path: &str) -> Url {
        self.origin
            .join(path)
            .expect("static CDP paths must be valid URLs")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserVersion {
    #[serde(rename = "Browser")]
    pub browser: Option<String>,

    #[serde(rename = "Protocol-Version")]
    pub protocol_version: Option<String>,

    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,

    #[serde(rename = "type")]
    pub target_type: String,

    pub title: String,
    pub url: String,

    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_cdp_discovery_urls() {
        let endpoint = CdpEndpoint::parse("http://127.0.0.1:9222").unwrap();

        assert_eq!(
            endpoint.version_url().as_str(),
            "http://127.0.0.1:9222/json/version"
        );
        assert_eq!(
            endpoint.targets_url().as_str(),
            "http://127.0.0.1:9222/json/list"
        );
    }
}
