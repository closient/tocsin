// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin node`: the key pair a node of a federation signs its pushes
//! with. `docs/config.md` describes where each half goes.

use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use tocsin_core::federation::NodeKey;

use crate::exit::Exit;
use crate::output::{Failure, Report};

/// `tocsin node keygen`: a new key pair from the system's random source.
/// The private key is printed here once and kept nowhere.
pub fn keygen() -> Result<Report, Failure> {
    let mut seed = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .map_err(|_| Failure::new(Exit::Runtime, "the system random source failed"))?;
    let key = NodeKey::from_seed(seed);
    let public_key = key.public_key().to_string();
    let private_key = key.expose();
    let text = format!(
        "public_key  = {public_key}\n\
         private_key = {private_key}\n\n\
         Put the public key in this node's `[[node]] public_keys`; it is safe to commit.\n\
         Put the private key in the secret store, as the environment variable\n\
         `[server] node_key` names, such as TOCSIN_NODE_KEY, on this node's host only.\n\
         Never in a file."
    );
    Ok(Report {
        text,
        exit: Exit::Ok,
        json: json!({ "public_key": public_key, "private_key": private_key }),
    })
}
