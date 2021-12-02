use crate::model::cfd::{Cet, Dlc, RevokedCommit, Role};
use crate::model::{Leverage, Price, Usd};
use crate::tokio_ext::FutureExt;
use crate::wire::{
    Msg0, Msg1, Msg2, RollOverMsg, RollOverMsg0, RollOverMsg1, RollOverMsg2, SetupMsg,
};
use crate::{model, oracle, payout_curve, wallet};
use anyhow::{anyhow, Context, Result};
use bdk::bitcoin::secp256k1::{schnorrsig, Signature, SECP256K1};
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::{Amount, PublicKey, Transaction};
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use futures::stream::FusedStream;
use futures::{Sink, SinkExt, StreamExt};
use maia::secp256k1_zkp::EcdsaAdaptorSignature;
use maia::{
    commit_descriptor, compute_adaptor_pk, create_cfd_transactions, interval, lock_descriptor,
    renew_cfd_transactions, secp256k1_zkp, spending_tx_sighash, Announcement, PartyParams,
    PunishParams,
};
use std::collections::HashMap;
use std::iter::FromIterator;
use std::ops::RangeInclusive;
use std::time::Duration;
use xtra::prelude::MessageChannel;

pub struct SetupParams {
    margin: Amount,
    counterparty_margin: Amount,
    price: Price,
    quantity: Usd,
    leverage: Leverage,
    refund_timelock: u32,
}

impl SetupParams {
    pub fn new(
        margin: Amount,
        counterparty_margin: Amount,
        price: Price,
        quantity: Usd,
        leverage: Leverage,
        refund_timelock: u32,
    ) -> Self {
        Self {
            margin,
            counterparty_margin,
            price,
            quantity,
            leverage,
            refund_timelock,
        }
    }
}

/// Given an initial set of parameters, sets up the CFD contract with
/// the other party.
#[allow(clippy::too_many_arguments)]
pub async fn new(
    mut sink: impl Sink<SetupMsg, Error = anyhow::Error> + Unpin,
    mut stream: impl FusedStream<Item = SetupMsg> + Unpin,
    (oracle_pk, announcement): (schnorrsig::PublicKey, oracle::Announcement),
    setup_params: SetupParams,
    build_party_params_channel: Box<dyn MessageChannel<wallet::BuildPartyParams>>,
    sign_channel: Box<dyn MessageChannel<wallet::Sign>>,
    role: Role,
    n_payouts: usize,
) -> Result<Dlc, ContractSetupError> {
    let (sk, pk) = crate::keypair::new(&mut rand::thread_rng());
    let (rev_sk, rev_pk) = crate::keypair::new(&mut rand::thread_rng());
    let (publish_sk, publish_pk) = crate::keypair::new(&mut rand::thread_rng());

    let own_params = build_party_params_channel
        .send(wallet::BuildPartyParams {
            amount: setup_params.margin,
            identity_pk: pk,
        })
        .await
        .context("Failed to send message to wallet actor")?
        .context("Failed to build party params")?;

    let own_punish = PunishParams {
        revocation_pk: rev_pk,
        publish_pk,
    };

    sink.send(SetupMsg::Msg0(Msg0::from((own_params.clone(), own_punish))))
        .await
        .context("Failed to send Msg0")?;
    let msg0 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg0 within 60 seconds")?
        .try_into_msg0()
        .context("Failed to read Msg0")?;

    tracing::info!("Exchanged setup parameters");

    let (other, other_punish) = msg0.into();

    let params = AllParams::new(own_params, own_punish, other, other_punish, role);

    if params.other.lock_amount != setup_params.counterparty_margin {
        return Err(ContractSetupError::FailedBeforeLockSignature {
            source: anyhow!(
                "Amounts sent by counterparty don't add up, expected margin {} but got {}",
                setup_params.counterparty_margin,
                params.other.lock_amount
            ),
        });
    }

    let settlement_event_id = announcement.id;
    let payouts = HashMap::from_iter([(
        announcement.into(),
        payout_curve::calculate(
            setup_params.price,
            setup_params.quantity,
            setup_params.leverage,
            n_payouts,
        )?,
    )]);

    let own_cfd_txs = create_cfd_transactions(
        (params.maker().clone(), *params.maker_punish()),
        (params.taker().clone(), *params.taker_punish()),
        oracle_pk,
        (model::cfd::Cfd::CET_TIMELOCK, setup_params.refund_timelock),
        payouts,
        sk,
    )
    .context("Failed to create CFD transactions")?;

    tracing::info!("Created CFD transactions");

    sink.send(SetupMsg::Msg1(Msg1::from(own_cfd_txs.clone())))
        .await
        .context("Failed to send Msg1")?;

    let msg1 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg1 within 60 seconds")?
        .try_into_msg1()
        .context("Failed to read Msg1")?;

    tracing::info!("Exchanged CFD transactions");

    let lock_desc = lock_descriptor(params.maker().identity_pk, params.taker().identity_pk);

    let lock_amount = params.maker().lock_amount + params.taker().lock_amount;

    let commit_desc = commit_descriptor(
        (
            params.maker().identity_pk,
            params.maker_punish().revocation_pk,
            params.maker_punish().publish_pk,
        ),
        (
            params.taker().identity_pk,
            params.taker_punish().revocation_pk,
            params.taker_punish().publish_pk,
        ),
    );

    let own_cets = own_cfd_txs.cets;
    let commit_tx = own_cfd_txs.commit.0.clone();

    let commit_amount = Amount::from_sat(commit_tx.output[0].value);

    verify_adaptor_signature(
        &commit_tx,
        &lock_desc,
        lock_amount,
        &msg1.commit,
        &params.own_punish.publish_pk,
        &params.other.identity_pk,
    )
    .context("Commit adaptor signature does not verify")?;

    for own_grouped_cets in &own_cets {
        let other_cets = msg1
            .cets
            .get(&own_grouped_cets.event.id)
            .context("Expect event to exist in msg")?;

        verify_cets(
            (&oracle_pk, &own_grouped_cets.event.nonce_pks),
            &params.other,
            own_grouped_cets.cets.as_slice(),
            other_cets.as_slice(),
            &commit_desc,
            commit_amount,
        )
        .context("CET signatures don't verify")?;
    }

    let lock_tx = own_cfd_txs.lock;
    let refund_tx = own_cfd_txs.refund.0;

    verify_signature(
        &refund_tx,
        &commit_desc,
        commit_amount,
        &msg1.refund,
        &params.other.identity_pk,
    )
    .context("Refund signature does not verify")?;

    tracing::info!("Verified all signatures");

    let cets = own_cets
        .into_iter()
        .map(|grouped_cets| {
            let event_id = grouped_cets.event.id;
            let other_cets = msg1
                .cets
                .get(&event_id)
                .with_context(|| format!("Counterparty CETs for event {} missing", event_id))?;
            let cets = grouped_cets
                .cets
                .into_iter()
                .map(|(tx, _, digits)| {
                    let other_encsig = other_cets
                        .iter()
                        .find_map(|(other_range, other_encsig)| {
                            (other_range == &digits.range()).then(|| other_encsig)
                        })
                        .with_context(|| {
                            format!(
                                "Missing counterparty adaptor signature for CET corresponding to
                                 price range {:?}",
                                digits.range()
                            )
                        })?;
                    Ok(Cet {
                        tx,
                        adaptor_sig: *other_encsig,
                        range: digits.range(),
                        n_bits: digits.len(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((event_id.parse()?, cets))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let mut signed_lock_tx = sign_channel
        .send(wallet::Sign { psbt: lock_tx })
        .await
        .context("Failed to send message to wallet actor")?
        .context("Failed to sign transaction")?;
    sink.send(SetupMsg::Msg2(Msg2 {
        signed_lock: signed_lock_tx.clone(),
    }))
    .await
    .context("Failed to send Msg2")?;

    let incomplete_dlc = Dlc {
        identity: sk,
        identity_counterparty: params.other.identity_pk,
        revocation: rev_sk,
        revocation_pk_counterparty: other_punish.revocation_pk,
        publish: publish_sk,
        publish_pk_counterparty: other_punish.publish_pk,
        maker_address: params.maker().address.clone(),
        taker_address: params.taker().address.clone(),
        lock: (signed_lock_tx.clone().extract_tx(), lock_desc.clone()),
        commit: (commit_tx, msg1.commit, commit_desc),
        cets,
        refund: (refund_tx, msg1.refund),
        maker_lock_amount: params.maker().lock_amount,
        taker_lock_amount: params.taker().lock_amount,
        revoked_commit: Vec::new(),
        settlement_event_id,
        refund_timelock: setup_params.refund_timelock,
    };

    let msg2 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg2 within 60 seconds")?
        .try_into_msg2()
        .context("Failed to read Msg2")
        .map_err(
            |e| ContractSetupError::LockTransactionSignatureSentButNotReceived {
                incomplete_dlc: incomplete_dlc.clone(),
                source: e,
            },
        )?;

    signed_lock_tx
        .merge(msg2.signed_lock)
        .context("Failed to merge lock PSBTs")
        .map_err(
            |e| ContractSetupError::LockTransactionSignatureSentButNotReceived {
                incomplete_dlc: incomplete_dlc.clone(),
                source: e,
            },
        )?;

    Ok(Dlc {
        lock: (signed_lock_tx.extract_tx(), lock_desc),
        ..incomplete_dlc
    })
}

#[derive(thiserror::Error, Debug)]
pub enum ContractSetupError {
    #[error("Contract setup failed, neither party is able to publish transactions")]
    FailedBeforeLockSignature {
        #[from]
        source: anyhow::Error,
    },
    #[error("The contract setup finished in an incomplete state, unable to publish lock transaction but the other party may publish it")]
    LockTransactionSignatureSentButNotReceived {
        incomplete_dlc: Dlc,
        source: anyhow::Error,
    },
}

pub struct RolloverParams {
    price: Price,
    quantity: Usd,
    leverage: Leverage,
    refund_timelock: u32,
}

impl RolloverParams {
    pub fn new(price: Price, quantity: Usd, leverage: Leverage, refund_timelock: u32) -> Self {
        Self {
            price,
            quantity,
            leverage,
            refund_timelock,
        }
    }
}

pub async fn roll_over(
    mut sink: impl Sink<RollOverMsg, Error = anyhow::Error> + Unpin,
    mut stream: impl FusedStream<Item = RollOverMsg> + Unpin,
    (oracle_pk, announcement): (schnorrsig::PublicKey, oracle::Announcement),
    rollover_params: RolloverParams,
    our_role: Role,
    dlc: Dlc,
    n_payouts: usize,
) -> Result<Dlc, RolloverError> {
    let sk = dlc.identity;
    let pk = PublicKey::new(secp256k1_zkp::PublicKey::from_secret_key(SECP256K1, &sk));

    let (rev_sk, rev_pk) = crate::keypair::new(&mut rand::thread_rng());
    let (publish_sk, publish_pk) = crate::keypair::new(&mut rand::thread_rng());

    let own_punish = PunishParams {
        revocation_pk: rev_pk,
        publish_pk,
    };

    sink.send(RollOverMsg::Msg0(RollOverMsg0 {
        revocation_pk: rev_pk,
        publish_pk,
    }))
    .await
    .context("Failed to send Msg0")?;
    let msg0 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg0 within 60 seconds")?
        .try_into_msg0()
        .context("Failed to read Msg0")?;

    let maker_lock_amount = dlc.maker_lock_amount;
    let taker_lock_amount = dlc.taker_lock_amount;
    let payouts = HashMap::from_iter([(
        // TODO : we want to support multiple announcements
        Announcement {
            id: announcement.id.to_string(),
            nonce_pks: announcement.nonce_pks.clone(),
        },
        payout_curve::calculate(
            rollover_params.price,
            rollover_params.quantity,
            rollover_params.leverage,
            n_payouts,
        )?,
    )]);

    // unsign lock tx because PartiallySignedTransaction needs an unsigned tx
    let mut unsigned_lock_tx = dlc.lock.0.clone();
    unsigned_lock_tx
        .input
        .iter_mut()
        .for_each(|input| input.witness.clear());

    let lock_tx = PartiallySignedTransaction::from_unsigned_tx(unsigned_lock_tx)
        .context("Unable to create partially signed transaction from unsigned tx")?;
    let other_punish_params = PunishParams {
        revocation_pk: msg0.revocation_pk,
        publish_pk: msg0.publish_pk,
    };
    let ((maker_identity, maker_punish_params), (taker_identity, taker_punish_params)) =
        match our_role {
            Role::Maker => (
                (pk, own_punish),
                (dlc.identity_counterparty, other_punish_params),
            ),
            Role::Taker => (
                (dlc.identity_counterparty, other_punish_params),
                (pk, own_punish),
            ),
        };
    let own_cfd_txs = renew_cfd_transactions(
        lock_tx.clone(),
        (
            maker_identity,
            maker_lock_amount,
            dlc.maker_address.clone(),
            maker_punish_params,
        ),
        (
            taker_identity,
            taker_lock_amount,
            dlc.taker_address.clone(),
            taker_punish_params,
        ),
        oracle_pk,
        (
            model::cfd::Cfd::CET_TIMELOCK,
            rollover_params.refund_timelock,
        ),
        payouts,
        sk,
    )
    .context("Failed to create new CFD transactions")?;

    sink.send(RollOverMsg::Msg1(RollOverMsg1::from(own_cfd_txs.clone())))
        .await
        .context("Failed to send Msg1")?;

    let msg1 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg1 within 60 seconds")?
        .try_into_msg1()
        .context("Failed to read Msg1")?;

    let lock_amount = taker_lock_amount + maker_lock_amount;

    let commit_desc = commit_descriptor(
        (
            maker_identity,
            maker_punish_params.revocation_pk,
            maker_punish_params.publish_pk,
        ),
        (
            taker_identity,
            taker_punish_params.revocation_pk,
            taker_punish_params.publish_pk,
        ),
    );

    let own_cets = own_cfd_txs.cets;
    let commit_tx = own_cfd_txs.commit.0.clone();

    let commit_amount = Amount::from_sat(commit_tx.output[0].value);

    verify_adaptor_signature(
        &commit_tx,
        &dlc.lock.1,
        lock_amount,
        &msg1.commit,
        &publish_pk,
        &dlc.identity_counterparty,
    )
    .context("Commit adaptor signature does not verify")?;

    let other_address = match our_role {
        Role::Maker => dlc.taker_address.clone(),
        Role::Taker => dlc.maker_address.clone(),
    };

    for own_grouped_cets in &own_cets {
        let other_cets = msg1
            .cets
            .get(&own_grouped_cets.event.id)
            .context("Expect event to exist in msg")?;

        verify_cets(
            (&oracle_pk, &announcement.nonce_pks),
            &PartyParams {
                lock_psbt: lock_tx.clone(),
                identity_pk: dlc.identity_counterparty,
                lock_amount,
                address: other_address.clone(),
            },
            own_grouped_cets.cets.as_slice(),
            other_cets.as_slice(),
            &commit_desc,
            commit_amount,
        )
        .context("CET signatures don't verify")?;
    }

    let refund_tx = own_cfd_txs.refund.0;

    verify_signature(
        &refund_tx,
        &commit_desc,
        commit_amount,
        &msg1.refund,
        &dlc.identity_counterparty,
    )
    .context("Refund signature does not verify")?;

    let cets = own_cets
        .into_iter()
        .map(|grouped_cets| {
            let event_id = grouped_cets.event.id;
            let other_cets = msg1
                .cets
                .get(&event_id)
                .with_context(|| format!("Counterparty CETs for event {} missing", event_id))?;
            let cets = grouped_cets
                .cets
                .into_iter()
                .map(|(tx, _, digits)| {
                    let other_encsig = other_cets
                        .iter()
                        .find_map(|(other_range, other_encsig)| {
                            (other_range == &digits.range()).then(|| other_encsig)
                        })
                        .with_context(|| {
                            format!(
                                "Missing counterparty adaptor signature for CET corresponding to
                                 price range {:?}",
                                digits.range()
                            )
                        })?;
                    Ok(Cet {
                        tx,
                        adaptor_sig: *other_encsig,
                        range: digits.range(),
                        n_bits: digits.len(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((event_id.parse()?, cets))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    // reveal revocation secrets to the other party
    sink.send(RollOverMsg::Msg2(RollOverMsg2 {
        revocation_sk: dlc.revocation,
    }))
    .await
    .context("Failed to send Msg2")?;

    let mut revoked_commit = dlc.revoked_commit;

    let incomplete_dlc = Dlc {
        identity: sk,
        identity_counterparty: dlc.identity_counterparty,
        revocation: rev_sk,
        revocation_pk_counterparty: other_punish_params.revocation_pk,
        publish: publish_sk,
        publish_pk_counterparty: other_punish_params.publish_pk,
        maker_address: dlc.maker_address,
        taker_address: dlc.taker_address,
        lock: dlc.lock.clone(),
        commit: (commit_tx, msg1.commit, commit_desc),
        cets,
        refund: (refund_tx, msg1.refund),
        maker_lock_amount,
        taker_lock_amount,
        revoked_commit: revoked_commit.clone(),
        settlement_event_id: announcement.id,
        refund_timelock: rollover_params.refund_timelock,
    };

    let msg2 = stream
        .select_next_some()
        .timeout(Duration::from_secs(60))
        .await
        .context("Expected Msg2 within 60 seconds")?
        .try_into_msg2()
        .context("Failed to read Msg2")
        .map_err(|e| RolloverError::RevocationSignaturesSentButNotReceived {
            incomplete_dlc: incomplete_dlc.clone(),
            source: e,
        })?;
    let revocation_sk_theirs = msg2.revocation_sk;

    {
        let derived_rev_pk = PublicKey::new(secp256k1_zkp::PublicKey::from_secret_key(
            SECP256K1,
            &revocation_sk_theirs,
        ));

        if derived_rev_pk != dlc.revocation_pk_counterparty {
            return Err(RolloverError::RevocationSignaturesSentButNotReceived {
                incomplete_dlc: incomplete_dlc.clone(),
                source: anyhow!("Counterparty sent invalid revocation sk"),
            });
        }
    }

    revoked_commit.push(RevokedCommit {
        encsig_ours: own_cfd_txs.commit.1,
        revocation_sk_theirs,
        publication_pk_theirs: dlc.publish_pk_counterparty,
        txid: dlc.commit.0.txid(),
        script_pubkey: dlc.commit.2.script_pubkey(),
    });

    Ok(Dlc {
        revoked_commit,
        ..incomplete_dlc
    })
}

/// A convenience struct for storing PartyParams and PunishParams of both
/// parties and the role of the caller.
struct AllParams {
    pub own: PartyParams,
    pub own_punish: PunishParams,
    pub other: PartyParams,
    pub other_punish: PunishParams,
    pub own_role: Role,
}

impl AllParams {
    fn new(
        own: PartyParams,
        own_punish: PunishParams,
        other: PartyParams,
        other_punish: PunishParams,
        own_role: Role,
    ) -> Self {
        Self {
            own,
            own_punish,
            other,
            other_punish,
            own_role,
        }
    }

    fn maker(&self) -> &PartyParams {
        match self.own_role {
            Role::Maker => &self.own,
            Role::Taker => &self.other,
        }
    }

    fn taker(&self) -> &PartyParams {
        match self.own_role {
            Role::Maker => &self.other,
            Role::Taker => &self.own,
        }
    }

    fn maker_punish(&self) -> &PunishParams {
        match self.own_role {
            Role::Maker => &self.own_punish,
            Role::Taker => &self.other_punish,
        }
    }
    fn taker_punish(&self) -> &PunishParams {
        match self.own_role {
            Role::Maker => &self.other_punish,
            Role::Taker => &self.own_punish,
        }
    }
}

fn verify_cets(
    oracle_params: (&schnorrsig::PublicKey, &[schnorrsig::PublicKey]),
    other: &PartyParams,
    own_cets: &[(Transaction, EcdsaAdaptorSignature, interval::Digits)],
    cets: &[(RangeInclusive<u64>, EcdsaAdaptorSignature)],
    commit_desc: &Descriptor<PublicKey>,
    commit_amount: Amount,
) -> Result<()> {
    for (tx, _, digits) in own_cets.iter() {
        let other_encsig = cets
            .iter()
            .find_map(|(range, encsig)| (range == &digits.range()).then(|| encsig))
            .with_context(|| {
                format!(
                    "no enc sig from other party for price range {:?}",
                    digits.range()
                )
            })?;

        verify_cet_encsig(
            tx,
            other_encsig,
            digits,
            &other.identity_pk,
            oracle_params,
            commit_desc,
            commit_amount,
        )
        .context("enc sig on CET does not verify")?;
    }
    Ok(())
}

fn verify_adaptor_signature(
    tx: &Transaction,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    encsig: &EcdsaAdaptorSignature,
    encryption_point: &PublicKey,
    pk: &PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);

    encsig
        .verify(SECP256K1, &sighash, &pk.key, &encryption_point.key)
        .context("failed to verify encsig spend tx")
}

fn verify_signature(
    tx: &Transaction,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    sig: &Signature,
    pk: &PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);
    SECP256K1.verify(&sighash, sig, &pk.key)?;
    Ok(())
}

fn verify_cet_encsig(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    digits: &interval::Digits,
    pk: &PublicKey,
    (oracle_pk, nonce_pks): (&schnorrsig::PublicKey, &[schnorrsig::PublicKey]),
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
) -> Result<()> {
    let index_nonce_pairs = &digits
        .to_indices()
        .into_iter()
        .zip(nonce_pks.iter().cloned())
        .collect::<Vec<_>>();
    let adaptor_point = compute_adaptor_pk(oracle_pk, index_nonce_pairs)
        .context("could not calculate adaptor point")?;
    verify_adaptor_signature(
        tx,
        spent_descriptor,
        spent_amount,
        encsig,
        &PublicKey::new(adaptor_point),
        pk,
    )
}

#[derive(thiserror::Error, Debug)]
pub enum RolloverError {
    // TODO: What do we do if this error happens? Is the CFD remains open (i.e. rollover is just
    // discarded)?
    #[error("Rollover failed")]
    FailedBeforeRevocationSignatures {
        #[from]
        source: anyhow::Error,
    },
    // TODO: What do we do if this error happens? Immediately send the commit tx of the new Dlc I
    // suppose - because we don't have guarantees against the old state without the revoke commit.
    #[error("The rollover finished in an incomplete state, unable to continue")]
    RevocationSignaturesSentButNotReceived {
        incomplete_dlc: Dlc,
        source: anyhow::Error,
    },
}
