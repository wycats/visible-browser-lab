use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use crate::{
    ipc::{BrokerEndpoint, BrokerStream},
    leases::{BrowserToolError, BrowserToolErrorCode},
};

pub const BROKER_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStatus {
    pub protocol_version: u32,
    pub pid: u32,
    pub cdp_endpoint: String,
    pub ipc_endpoint: String,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BrowserToolError>,
}

impl BrokerResponse {
    pub fn success<T: Serialize>(id: String, result: T) -> Result<Self> {
        Ok(Self {
            id,
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn error(id: String, error: BrowserToolError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn invalid_input(id: String, message: impl Into<String>) -> Self {
        Self::error(id, BrowserToolError::invalid_input(message))
    }
}

pub struct BrokerClient {
    stream: BufReader<BrokerStream>,
}

impl BrokerClient {
    pub async fn connect(endpoint: &BrokerEndpoint) -> Result<Self> {
        let stream = endpoint.connect().await?;
        Ok(Self::new(stream))
    }

    pub fn new(stream: BrokerStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    pub async fn ping(&mut self) -> Result<BrokerStatus> {
        self.request("ping", Value::Null).await
    }

    pub async fn request<P, R>(&mut self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let request_id = Uuid::new_v4().to_string();
        let request = BrokerRequest {
            id: request_id.clone(),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };
        let encoded = serde_json::to_string(&request)?;

        self.stream.get_mut().write_all(encoded.as_bytes()).await?;
        self.stream.get_mut().write_all(b"\n").await?;
        self.stream.get_mut().flush().await?;

        let mut line = String::new();
        let bytes = self.stream.read_line(&mut line).await?;
        if bytes == 0 {
            bail!("broker closed the socket before responding to `{method}`");
        }

        let response: BrokerResponse =
            serde_json::from_str(&line).context("broker returned invalid JSON")?;

        if response.id != request_id {
            bail!(
                "broker response id mismatch: expected `{request_id}`, got `{}`",
                response.id
            );
        }

        if response.ok {
            let result = response
                .result
                .context("broker success response omitted result")?;
            return Ok(serde_json::from_value(result)?);
        }

        let error = response.error.unwrap_or_else(|| BrowserToolError {
            code: BrowserToolErrorCode::InvalidInput,
            message: "broker error response omitted error payload".to_string(),
            recovery: None,
        });
        bail!(
            "broker `{method}` failed with {:?}: {}",
            error.code,
            error.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn success_response_omits_error_payload() {
        let response =
            BrokerResponse::success("request-1".to_string(), json!({ "ok": true })).unwrap();
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["ok"], true);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn error_response_omits_result_payload() {
        let response = BrokerResponse::invalid_input("request-1".to_string(), "bad request");
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "invalid_input");
        assert!(value.get("result").is_none());
    }
}
