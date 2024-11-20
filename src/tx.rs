use anyhow::{Context, Result};
use celestia_types::Blob;
use clap::Subcommand;
use prism_common::keys::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

/// If true, the system will verify signatures on transactions. If false,
/// signatures will be ignored.
pub const SIGNATURE_VERIFICATION_ENABLED: bool = false;

/// Represents the full set of transaction types supported by the system.
#[derive(Subcommand, Clone, Serialize, Deserialize, Debug)]
pub enum TransactionType {
    Noop,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Transaction {
    /// Signature of bincode::serialize(&(tx_type, nonce))
    /// For toy rollups or experimentation, use [`Signature::Placeholder`]
    pub signature: Signature,

    /// Account key of user.
    pub vk: VerifyingKey,

    /// Nonce of the account.
    /// If you want to prevent replay attacks, you MUST explicitly enforce
    /// nonces are strictly increasing in your [`State`].
    pub nonce: u64,

    /// Transaction variant.
    pub tx_type: TransactionType,
}

impl Transaction {
    pub fn verify(&self) -> Result<()> {
        if SIGNATURE_VERIFICATION_ENABLED {
            let msg = bincode::serialize(&(self.tx_type.clone(), self.nonce))?;
            self.vk.verify_signature(&msg, &self.signature)?;
        }

        match self.tx_type {
            TransactionType::Noop => Ok(()),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Batch(Vec<Transaction>);

impl Batch {
    pub fn new(txs: Vec<Transaction>) -> Self {
        Batch(txs.clone())
    }

    pub fn get_transactions(&self) -> Vec<Transaction> {
        self.0.clone()
    }
}

impl TryFrom<&Blob> for Batch {
    type Error = anyhow::Error;

    fn try_from(value: &Blob) -> Result<Self, Self::Error> {
        match bincode::deserialize(&value.data) {
            Ok(batch) => Ok(batch),
            Err(_) => {
                let transaction: Transaction = bincode::deserialize(&value.data)
                    .context(format!("Failed to decode blob into Transaction: {value:?}"))?;

                Ok(Batch(vec![transaction]))
            }
        }
    }
}
