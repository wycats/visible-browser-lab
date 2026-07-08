use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::protocol::BROKER_PROTOCOL_VERSION;
use anyhow::{Context, Result};
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions, Name,
    tokio::{Listener, Stream, prelude::*},
};

pub type BrokerListener = Listener;
pub type BrokerStream = Stream;

#[derive(Debug, Clone)]
pub struct BrokerEndpoint {
    display: String,
    stale_path: Option<PathBuf>,
    name: Arc<Name<'static>>,
}

impl BrokerEndpoint {
    pub fn from_state(state_dir: &Path, override_endpoint: Option<&str>) -> Result<Self> {
        let display = override_endpoint
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_endpoint_display(state_dir));

        let name = endpoint_name(&display)
            .with_context(|| format!("invalid broker IPC endpoint `{display}`"))?;
        let stale_path = stale_path_for_endpoint(&display);

        Ok(Self {
            display,
            stale_path,
            name: Arc::new(name),
        })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn stale_path(&self) -> Option<&Path> {
        self.stale_path.as_deref()
    }

    pub async fn connect(&self) -> Result<BrokerStream> {
        Stream::connect(self.name.borrow())
            .await
            .with_context(|| format!("failed to connect to broker IPC `{}`", self.display))
    }

    pub fn listen(&self) -> Result<BrokerListener> {
        ListenerOptions::new()
            .name(self.name.borrow())
            .create_tokio()
            .with_context(|| format!("failed to listen on broker IPC `{}`", self.display))
    }
}

pub async fn accept(listener: &BrokerListener) -> std::io::Result<BrokerStream> {
    listener.accept().await
}

#[cfg(windows)]
pub fn default_endpoint_display(state_dir: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    state_dir.hash(&mut hasher);
    format!(
        "visible-browser-lab-v{BROKER_PROTOCOL_VERSION}-{:016x}",
        hasher.finish()
    )
}

#[cfg(not(windows))]
pub fn default_endpoint_display(state_dir: &Path) -> String {
    state_dir
        .join(format!("broker-v{BROKER_PROTOCOL_VERSION}.sock"))
        .to_string_lossy()
        .into_owned()
}

fn endpoint_name(display: &str) -> Result<Name<'static>> {
    #[cfg(windows)]
    {
        Ok(display.to_ns_name::<GenericNamespaced>()?.into_owned())
    }

    #[cfg(not(windows))]
    {
        Ok(Path::new(display)
            .to_fs_name::<GenericFilePath>()?
            .into_owned())
    }
}

fn stale_path_for_endpoint(display: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let _ = display;
        None
    }

    #[cfg(not(windows))]
    {
        Some(PathBuf::from(display))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoint_uses_state_socket_path_on_unix() {
        if cfg!(windows) {
            return;
        }

        let endpoint = default_endpoint_display(Path::new("/tmp/visible-browser-lab-test"));

        assert_eq!(endpoint, "/tmp/visible-browser-lab-test/broker-v4.sock");
    }

    #[test]
    fn endpoint_tracks_stale_path_on_unix_only() {
        let endpoint =
            BrokerEndpoint::from_state(Path::new("/tmp/visible-browser-lab-test"), None).unwrap();

        if cfg!(windows) {
            assert!(endpoint.stale_path().is_none());
        } else {
            assert_eq!(
                endpoint.stale_path(),
                Some(Path::new("/tmp/visible-browser-lab-test/broker-v4.sock"))
            );
        }
    }
}
