use std::{
    io::{Read, Write},
    path::PathBuf,
};

use color_eyre::eyre::Result;
use once_cell::sync::Lazy;
use postcard::{from_bytes, to_allocvec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Packet {
    AddService(crate::daemon::Service),
    // TODO: start stop and delete
}

impl Packet {
    pub fn build(&self) -> Result<Vec<u8>> {
        let mut result = vec![];

        let postcard_bin = to_allocvec(&self)?;

        result.write_all(&(postcard_bin.len() as u16).to_be_bytes())?;

        result.write_all(&postcard_bin)?;

        Ok(result)
    }

    pub fn build_and_write<T: Write>(&self, stream: &mut T) -> Result<()> {
        let packet_bin = self.build()?;

        stream.write_all(&packet_bin)?;

        Ok(())
    }

    pub fn from_stream(stream: &mut impl Read) -> Result<Self> {
        let mut buf = [0u8, 0u8];

        stream.read_exact(&mut buf)?;
        let len = u16::from_be_bytes(buf);

        let mut buf = vec![0u8; len as usize];
        stream.read_exact(&mut buf)?;
        let packet: Self = from_bytes(&buf)?;

        Ok(packet)
    }
}

pub static SOCKET_PATH: Lazy<PathBuf> = Lazy::new(|| {
    let linux_user_id = nix::unistd::getuid().to_string();
    PathBuf::from(&format!("/var/run/user/{}/rsm.sock", linux_user_id))
});
