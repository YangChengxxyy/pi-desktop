//! Client side of pi's RPC mode (`pi --mode rpc`): wire types in [`wire`],
//! the blocking stdio transport in [`client`]. Protocol reference: pi's
//! `docs/rpc.md`, `docs/rpc-commands.md`, `docs/json.md`,
//! `docs/rpc-extension-ui.md`.

pub mod client;
pub mod wire;

pub use client::{Client, Error, Signal, signal_group};
pub use wire::*;
