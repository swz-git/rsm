use std::path::PathBuf;

use color_eyre::eyre::Result;
use once_cell::sync::Lazy;
use postcard::{from_bytes, to_allocvec};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::daemon::{
    service_threads::{ServiceState, ServiceThreadCommand},
    Service,
};

#[derive(Debug, Serialize, Deserialize)]
pub enum Packet {
    AddService(crate::daemon::Service),
    AddServiceResponse(Result<(), String>),
    RunCommand(String, ServiceThreadCommand),
    RunCommandResponse(Result<ServiceState, String>),
    ServicesInfo(),
    ServicesInfoResponse(Result<Vec<(Service, ServiceState)>, String>),
}

impl Packet {
    pub fn build(&self) -> Result<Vec<u8>> {
        let mut result = vec![];

        let postcard_bin = to_allocvec(&self)?;

        std::io::Write::write_all(&mut result, &(postcard_bin.len() as u16).to_be_bytes())?;

        std::io::Write::write_all(&mut result, &postcard_bin)?;

        Ok(result)
    }

    pub async fn build_and_write<T: tokio::io::AsyncWrite + Unpin>(
        &self,
        stream: &mut T,
    ) -> Result<()> {
        let packet_bin = self.build()?;

        tokio::io::AsyncWriteExt::write_all(stream, &packet_bin).await?;

        Ok(())
    }

    pub async fn from_stream<T: AsyncRead + Unpin>(stream: &mut T) -> Result<Self> {
        let mut buf = [0u8, 0u8];

        AsyncReadExt::read_exact(stream, &mut buf).await?;
        let len = u16::from_be_bytes(buf);

        let mut buf = vec![0u8; len as usize];
        AsyncReadExt::read_exact(stream, &mut buf).await?;
        let packet: Self = from_bytes(&buf)?;

        Ok(packet)
    }
}

pub static SOCKET_PATH: Lazy<PathBuf> = Lazy::new(|| {
    let linux_user_id = nix::unistd::getuid().to_string();
    PathBuf::from(&format!("/var/run/user/{}/rsm.sock", linux_user_id))
});
