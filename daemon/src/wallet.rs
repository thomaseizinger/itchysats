use crate::model::WalletInfo;
use anyhow::{anyhow, Context, Result};
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::{Amount, PublicKey, Transaction, Txid};
use bdk::blockchain::{ElectrumBlockchain, NoopProgress};
use bdk::wallet::AddressIndex;
use bdk::{electrum_client, Error, KeychainKind, SignOptions};
use cfd_protocol::{PartyParams, WalletExt};
use rocket::serde::json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;

const SLED_TREE_NAME: &str = "wallet";

#[derive(Clone)]
pub struct Wallet {
    wallet: Arc<Mutex<bdk::Wallet<ElectrumBlockchain, bdk::sled::Tree>>>,
}

#[derive(thiserror::Error, Debug, Clone, Copy)]
#[error("The transaction is already in the blockchain")]
pub struct TransactionAlreadyInBlockchain;

impl Wallet {
    pub async fn new(
        electrum_rpc_url: &str,
        wallet_dir: &Path,
        ext_priv_key: ExtendedPrivKey,
    ) -> Result<Self> {
        let client = bdk::electrum_client::Client::new(electrum_rpc_url)
            .context("Failed to initialize Electrum RPC client")?;

        // TODO: Replace with sqlite once https://github.com/bitcoindevkit/bdk/pull/376 is merged.
        let db = bdk::sled::open(wallet_dir)?.open_tree(SLED_TREE_NAME)?;

        let wallet = bdk::Wallet::new(
            bdk::template::Bip84(ext_priv_key, KeychainKind::External),
            Some(bdk::template::Bip84(ext_priv_key, KeychainKind::Internal)),
            ext_priv_key.network,
            db,
            ElectrumBlockchain::from(client),
        )?;

        let wallet = Arc::new(Mutex::new(wallet));

        Ok(Self { wallet })
    }

    pub async fn build_party_params(
        &self,
        amount: Amount,
        identity_pk: PublicKey,
    ) -> Result<PartyParams> {
        let wallet = self.wallet.lock().await;
        wallet.build_party_params(amount, identity_pk)
    }

    pub async fn sync(&self) -> Result<WalletInfo> {
        let wallet = self.wallet.lock().await;
        wallet.sync(NoopProgress, None)?;

        let balance = wallet.get_balance()?;

        let address = wallet.get_address(AddressIndex::LastUnused)?.address;

        let wallet_info = WalletInfo {
            balance: Amount::from_sat(balance),
            address,
            last_updated_at: SystemTime::now(),
        };

        Ok(wallet_info)
    }

    pub async fn sign(
        &self,
        mut psbt: PartiallySignedTransaction,
    ) -> Result<PartiallySignedTransaction> {
        let wallet = self.wallet.lock().await;

        wallet
            .sign(
                &mut psbt,
                SignOptions {
                    trust_witness_utxo: true,
                    ..Default::default()
                },
            )
            .context("could not sign transaction")?;

        Ok(psbt)
    }

    pub async fn try_broadcast_transaction(&self, tx: Transaction) -> Result<Txid> {
        let wallet = self.wallet.lock().await;
        // TODO: Optimize this match to be a map_err / more readable in general
        let txid = tx.txid();
        match wallet.broadcast(tx) {
            Ok(txid) => Ok(txid),
            Err(e) => {
                if let Error::Electrum(electrum_client::Error::Protocol(ref value)) = e {
                    let error_code = match parse_rpc_protocol_error_code(value) {
                        Ok(error_code) => error_code,
                        Err(inner) => {
                            tracing::error!(
                                "Failed to parse error code from RPC message: {}",
                                inner
                            );
                            return Err(anyhow!(e));
                        }
                    };

                    if error_code == i64::from(RpcErrorCode::RpcVerifyAlreadyInChain) {
                        return Ok(txid);
                    }
                }

                Err(anyhow!(e))
            }
        }
    }
}

fn parse_rpc_protocol_error_code(error_value: &Value) -> Result<i64> {
    let json = error_value
        .as_str()
        .context("Not a string")?
        .split_terminator("RPC error: ")
        .nth(1)
        .context("Unknown error code format")?;

    let error = serde_json::from_str::<RpcError>(json).context("Error has unexpected format")?;

    Ok(error.code)
}

#[derive(serde::Deserialize)]
struct RpcError {
    code: i64,
}

/// Bitcoin error codes: https://github.com/bitcoin/bitcoin/blob/97d3500601c1d28642347d014a6de1e38f53ae4e/src/rpc/protocol.h#L23
pub enum RpcErrorCode {
    /// Transaction or block was rejected by network rules. Error code -27.
    RpcVerifyAlreadyInChain,
}

impl From<RpcErrorCode> for i64 {
    fn from(code: RpcErrorCode) -> Self {
        match code {
            RpcErrorCode::RpcVerifyAlreadyInChain => -27,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_response() {
        let response = serde_json::Value::String(r#"sendrawtransaction RPC error: {"code":-27,"message":"Transaction already in block chain"}"#.to_owned());

        let code = parse_rpc_protocol_error_code(&response).unwrap();

        assert_eq!(code, -27);
    }
}
