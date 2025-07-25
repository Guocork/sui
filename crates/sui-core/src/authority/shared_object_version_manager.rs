// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::authority::authority_per_epoch_store::CancelConsensusCertificateReason;
use crate::authority::AuthorityPerEpochStore;
use crate::execution_cache::ObjectCacheRead;
use either::Either;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use sui_types::base_types::ConsensusObjectSequenceKey;
use sui_types::base_types::TransactionDigest;
use sui_types::committee::EpochId;
use sui_types::crypto::RandomnessRound;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::executable_transaction::VerifiedExecutableTransaction;
use sui_types::storage::{
    transaction_non_shared_input_object_keys, transaction_receiving_object_keys, ObjectKey,
};
use sui_types::transaction::{SharedInputObject, TransactionDataAPI, TransactionKey};
use sui_types::SUI_ACCUMULATOR_ROOT_OBJECT_ID;
use sui_types::{base_types::SequenceNumber, error::SuiResult, SUI_RANDOMNESS_STATE_OBJECT_ID};
use tracing::trace;

use super::epoch_start_configuration::EpochStartConfigTrait;

pub struct SharedObjVerManager {}

pub type AssignedVersions = Vec<(ConsensusObjectSequenceKey, SequenceNumber)>;

#[derive(Default, Debug)]
pub struct AssignedTxAndVersions(pub Vec<(TransactionKey, AssignedVersions)>);

impl AssignedTxAndVersions {
    pub fn new(assigned_versions: Vec<(TransactionKey, AssignedVersions)>) -> Self {
        Self(assigned_versions)
    }

    pub fn into_map(self) -> HashMap<TransactionKey, AssignedVersions> {
        self.0.into_iter().collect()
    }
}

/// A wrapper around things that can be scheduled for execution by the assigning of
/// shared object versions.
#[derive(Clone)]
pub enum Schedulable<T = VerifiedExecutableTransaction> {
    Transaction(T),
    RandomnessStateUpdate(EpochId, RandomnessRound),
    // For a transaction that has withdraw reservations,
    // pass the transaction and the accumulator version it depends on.
    Withdraw(T, SequenceNumber),
    AccumulatorSettlement(EpochId, u64 /* checkpoint height */),
}

impl From<VerifiedExecutableTransaction> for Schedulable<VerifiedExecutableTransaction> {
    fn from(tx: VerifiedExecutableTransaction) -> Self {
        Schedulable::Transaction(tx)
    }
}

// AsTx is like Deref, in that it allows us to use either refs or values in Schedulable.
// Deref does not work because it conflicts with the impl of Deref for VerifiedExecutableTransaction.
pub trait AsTx {
    fn as_tx(&self) -> &VerifiedExecutableTransaction;
}

impl AsTx for VerifiedExecutableTransaction {
    fn as_tx(&self) -> &VerifiedExecutableTransaction {
        self
    }
}

impl AsTx for &'_ VerifiedExecutableTransaction {
    fn as_tx(&self) -> &VerifiedExecutableTransaction {
        self
    }
}

impl Schedulable<&'_ VerifiedExecutableTransaction> {
    // Cannot use the blanket ToOwned trait impl because it just calls clone.
    pub fn to_owned_schedulable(&self) -> Schedulable<VerifiedExecutableTransaction> {
        match self {
            Schedulable::Transaction(tx) => Schedulable::Transaction((*tx).clone()),
            Schedulable::RandomnessStateUpdate(epoch, round) => {
                Schedulable::RandomnessStateUpdate(*epoch, *round)
            }
            Schedulable::Withdraw(tx, version) => Schedulable::Withdraw((*tx).clone(), *version),
            Schedulable::AccumulatorSettlement(epoch, checkpoint_height) => {
                Schedulable::AccumulatorSettlement(*epoch, *checkpoint_height)
            }
        }
    }
}

impl<T> Schedulable<T> {
    pub fn as_tx(&self) -> Option<&VerifiedExecutableTransaction>
    where
        T: AsTx,
    {
        match self {
            Schedulable::Transaction(tx) | Schedulable::Withdraw(tx, _) => Some(tx.as_tx()),
            Schedulable::RandomnessStateUpdate(_, _) => None,
            Schedulable::AccumulatorSettlement(_, _) => None,
        }
    }

    pub fn shared_input_objects(
        &self,
        epoch_store: &AuthorityPerEpochStore,
    ) -> impl Iterator<Item = SharedInputObject> + '_
    where
        T: AsTx,
    {
        match self {
            Schedulable::Transaction(tx) | Schedulable::Withdraw(tx, _) => {
                Either::Left(tx.as_tx().shared_input_objects())
            }
            Schedulable::RandomnessStateUpdate(_, _) => {
                Either::Right(std::iter::once(SharedInputObject {
                    id: SUI_RANDOMNESS_STATE_OBJECT_ID,
                    initial_shared_version: epoch_store
                        .epoch_start_config()
                        .randomness_obj_initial_shared_version()
                        .expect("randomness obj initial shared version should be set"),
                    mutable: true,
                }))
            }
            Schedulable::AccumulatorSettlement(_, _) => {
                Either::Right(std::iter::once(SharedInputObject {
                    id: SUI_ACCUMULATOR_ROOT_OBJECT_ID,
                    initial_shared_version: epoch_store
                        .epoch_start_config()
                        .accumulator_root_obj_initial_shared_version()
                        .expect("accumulator root obj initial shared version should be set"),
                    mutable: true,
                }))
            }
        }
    }

    pub fn non_shared_input_object_keys(&self) -> Vec<ObjectKey>
    where
        T: AsTx,
    {
        match self {
            Schedulable::Transaction(tx) | Schedulable::Withdraw(tx, _) => {
                transaction_non_shared_input_object_keys(tx.as_tx())
                    .expect("Transaction input should have been verified")
            }
            Schedulable::RandomnessStateUpdate(_, _) => vec![],
            Schedulable::AccumulatorSettlement(_, _) => vec![],
        }
    }

    pub fn receiving_object_keys(&self) -> Vec<ObjectKey>
    where
        T: AsTx,
    {
        match self {
            Schedulable::Transaction(tx) | Schedulable::Withdraw(tx, _) => {
                transaction_receiving_object_keys(tx.as_tx())
            }
            Schedulable::RandomnessStateUpdate(_, _) => vec![],
            Schedulable::AccumulatorSettlement(_, _) => vec![],
        }
    }

    pub fn key(&self) -> TransactionKey
    where
        T: AsTx,
    {
        match self {
            Schedulable::Transaction(tx) | Schedulable::Withdraw(tx, _) => tx.as_tx().key(),
            Schedulable::RandomnessStateUpdate(epoch, round) => {
                TransactionKey::RandomnessRound(*epoch, *round)
            }
            Schedulable::AccumulatorSettlement(epoch, checkpoint_height) => {
                TransactionKey::AccumulatorSettlement(*epoch, *checkpoint_height)
            }
        }
    }
}

#[must_use]
#[derive(Default)]
pub struct ConsensusSharedObjVerAssignment {
    pub shared_input_next_versions: HashMap<ConsensusObjectSequenceKey, SequenceNumber>,
    pub assigned_versions: AssignedTxAndVersions,
}

impl SharedObjVerManager {
    pub fn assign_versions_from_consensus<'a, T>(
        epoch_store: &AuthorityPerEpochStore,
        cache_reader: &dyn ObjectCacheRead,
        assignables: impl Iterator<Item = &'a Schedulable<T>> + Clone,
        cancelled_txns: &BTreeMap<TransactionDigest, CancelConsensusCertificateReason>,
    ) -> SuiResult<ConsensusSharedObjVerAssignment>
    where
        T: AsTx + 'a,
    {
        let mut shared_input_next_versions = get_or_init_versions(
            assignables
                .clone()
                .flat_map(|a| a.shared_input_objects(epoch_store)),
            epoch_store,
            cache_reader,
        )?;
        let mut assigned_versions = Vec::new();
        for assignable in assignables {
            assert!(
                !matches!(assignable, Schedulable::AccumulatorSettlement(_, _))
                    || epoch_store.accumulators_enabled(),
                "AccumulatorSettlement should not be scheduled when accumulators are disabled"
            );

            let cert_assigned_versions = Self::assign_versions_for_certificate(
                epoch_store,
                assignable,
                &mut shared_input_next_versions,
                cancelled_txns,
            );
            assigned_versions.push((assignable.key(), cert_assigned_versions));
        }

        Ok(ConsensusSharedObjVerAssignment {
            shared_input_next_versions,
            assigned_versions: AssignedTxAndVersions::new(assigned_versions),
        })
    }

    pub fn assign_versions_from_effects(
        certs_and_effects: &[(&VerifiedExecutableTransaction, &TransactionEffects)],
        epoch_store: &AuthorityPerEpochStore,
        cache_reader: &dyn ObjectCacheRead,
    ) -> AssignedTxAndVersions {
        // We don't care about the results since we can use effects to assign versions.
        // But we must call it to make sure whenever a shared object is touched the first time
        // during an epoch, either through consensus or through checkpoint executor,
        // its next version must be initialized. This is because we initialize the next version
        // of a shared object in an epoch by reading the current version from the object store.
        // This must be done before we mutate it the first time, otherwise we would be initializing
        // it with the wrong version.
        let _ = get_or_init_versions(
            certs_and_effects
                .iter()
                .flat_map(|(cert, _)| cert.transaction_data().shared_input_objects().into_iter()),
            epoch_store,
            cache_reader,
        );
        let mut assigned_versions = Vec::new();
        for (cert, effects) in certs_and_effects {
            let initial_version_map: BTreeMap<_, _> = cert
                .transaction_data()
                .shared_input_objects()
                .into_iter()
                .map(|input| input.into_id_and_version())
                .collect();
            let cert_assigned_versions: Vec<_> = effects
                .input_shared_objects()
                .into_iter()
                .map(|iso| {
                    let (id, version) = iso.id_and_version();
                    let initial_version = initial_version_map
                        .get(&id)
                        .expect("transaction must have all inputs from effects");
                    ((id, *initial_version), version)
                })
                .collect();
            let tx_key = cert.key();
            trace!(
                ?tx_key,
                ?cert_assigned_versions,
                "assigned shared object versions from effects"
            );
            assigned_versions.push((tx_key, cert_assigned_versions));
        }
        AssignedTxAndVersions::new(assigned_versions)
    }

    pub fn assign_versions_for_certificate(
        epoch_store: &AuthorityPerEpochStore,
        assignable: &Schedulable<impl AsTx>,
        shared_input_next_versions: &mut HashMap<ConsensusObjectSequenceKey, SequenceNumber>,
        cancelled_txns: &BTreeMap<TransactionDigest, CancelConsensusCertificateReason>,
    ) -> AssignedVersions {
        let shared_input_objects: Vec<_> = assignable.shared_input_objects(epoch_store).collect();
        if shared_input_objects.is_empty() {
            // No shared object used by this transaction. No need to assign versions.
            return vec![];
        }

        let tx_key = assignable.key();

        // Check if the transaction is cancelled due to congestion.
        let cancellation_info = tx_key
            .as_digest()
            .and_then(|tx_digest| cancelled_txns.get(tx_digest));
        let congested_objects_info: Option<HashSet<_>> =
            if let Some(CancelConsensusCertificateReason::CongestionOnObjects(congested_objects)) =
                &cancellation_info
            {
                Some(congested_objects.iter().cloned().collect())
            } else {
                None
            };
        let txn_cancelled = cancellation_info.is_some();

        let mut input_object_keys = assignable.non_shared_input_object_keys();
        let mut assigned_versions = Vec::with_capacity(shared_input_objects.len());
        let mut is_mutable_input = Vec::with_capacity(shared_input_objects.len());
        // Record receiving object versions towards the shared version computation.
        let receiving_object_keys = assignable.receiving_object_keys();
        input_object_keys.extend(receiving_object_keys);

        if txn_cancelled {
            // For cancelled transaction due to congestion, assign special versions to all shared objects.
            // Note that new lamport version does not depend on any shared objects.
            for SharedInputObject {
                id,
                initial_shared_version,
                ..
            } in shared_input_objects.iter()
            {
                let assigned_version = match cancellation_info {
                    Some(CancelConsensusCertificateReason::CongestionOnObjects(_)) => {
                        if congested_objects_info
                            .as_ref()
                            .is_some_and(|info| info.contains(id))
                        {
                            SequenceNumber::CONGESTED
                        } else {
                            SequenceNumber::CANCELLED_READ
                        }
                    }
                    Some(CancelConsensusCertificateReason::DkgFailed) => {
                        if id == &SUI_RANDOMNESS_STATE_OBJECT_ID {
                            SequenceNumber::RANDOMNESS_UNAVAILABLE
                        } else {
                            SequenceNumber::CANCELLED_READ
                        }
                    }
                    None => unreachable!("cancelled transaction should have cancellation info"),
                };
                assigned_versions.push(((*id, *initial_shared_version), assigned_version));
                is_mutable_input.push(false);
            }
        } else {
            for (
                SharedInputObject {
                    id,
                    initial_shared_version,
                    mutable,
                },
                assigned_version,
            ) in shared_input_objects.iter().map(|obj| {
                (
                    obj,
                    *shared_input_next_versions
                        .get(&obj.id_and_version())
                        .unwrap(),
                )
            }) {
                assigned_versions.push(((*id, *initial_shared_version), assigned_version));
                input_object_keys.push(ObjectKey(*id, assigned_version));
                is_mutable_input.push(*mutable);
            }
        }

        let next_version =
            SequenceNumber::lamport_increment(input_object_keys.iter().map(|obj| obj.1));
        assert!(
            next_version.is_valid(),
            "Assigned version must be valid. Got {:?}",
            next_version
        );

        if !txn_cancelled {
            // Update the next version for the shared objects.
            assigned_versions
                .iter()
                .zip(is_mutable_input)
                .filter_map(|((id, _), mutable)| {
                    if mutable {
                        Some((*id, next_version))
                    } else {
                        None
                    }
                })
                .for_each(|(id, version)| {
                    assert!(
                        version.is_valid(),
                        "Assigned version must be a valid version."
                    );
                    shared_input_next_versions
                        .insert(id, version)
                        .expect("Object must exist in shared_input_next_versions.");
                });
        }

        trace!(
            ?tx_key,
            ?assigned_versions,
            ?next_version,
            ?txn_cancelled,
            "locking shared objects"
        );

        assigned_versions
    }
}

fn get_or_init_versions<'a>(
    shared_input_objects: impl Iterator<Item = SharedInputObject> + 'a,
    epoch_store: &AuthorityPerEpochStore,
    cache_reader: &dyn ObjectCacheRead,
) -> SuiResult<HashMap<ConsensusObjectSequenceKey, SequenceNumber>> {
    let mut shared_input_objects: Vec<_> = shared_input_objects
        .map(|so| so.into_id_and_version())
        .collect();

    shared_input_objects.sort();
    shared_input_objects.dedup();

    epoch_store.get_or_init_next_object_versions(&shared_input_objects, cache_reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::authority::epoch_start_configuration::EpochStartConfigTrait;
    use crate::authority::shared_object_version_manager::{
        ConsensusSharedObjVerAssignment, SharedObjVerManager,
    };
    use crate::authority::test_authority_builder::TestAuthorityBuilder;
    use std::collections::{BTreeMap, HashMap};
    use sui_test_transaction_builder::TestTransactionBuilder;
    use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress};
    use sui_types::crypto::RandomnessRound;
    use sui_types::digests::ObjectDigest;
    use sui_types::effects::TestEffectsBuilder;
    use sui_types::executable_transaction::{
        CertificateProof, ExecutableTransaction, VerifiedExecutableTransaction,
    };
    use sui_types::object::Object;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::{ObjectArg, SenderSignedData, VerifiedTransaction};
    use sui_types::SUI_RANDOMNESS_STATE_OBJECT_ID;

    #[tokio::test]
    async fn test_assign_versions_from_consensus_basic() {
        let shared_object = Object::shared_for_testing();
        let id = shared_object.id();
        let init_shared_version = shared_object.owner.start_version().unwrap();
        let authority = TestAuthorityBuilder::new()
            .with_starting_objects(&[shared_object.clone()])
            .build()
            .await;
        let certs = vec![
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 3),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, false)], 5),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 9),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 11),
        ];
        let epoch_store = authority.epoch_store_for_testing();
        let assignables = certs
            .iter()
            .map(Schedulable::Transaction)
            .collect::<Vec<_>>();
        let ConsensusSharedObjVerAssignment {
            shared_input_next_versions,
            assigned_versions,
        } = SharedObjVerManager::assign_versions_from_consensus(
            &epoch_store,
            authority.get_object_cache_reader().as_ref(),
            assignables.iter(),
            &BTreeMap::new(),
        )
        .unwrap();
        // Check that the shared object's next version is always initialized in the epoch store.
        assert_eq!(
            epoch_store
                .get_next_object_version(&id, init_shared_version)
                .unwrap(),
            init_shared_version
        );
        // Check that the final version of the shared object is the lamport version of the last
        // transaction.
        assert_eq!(
            shared_input_next_versions,
            HashMap::from([((id, init_shared_version), SequenceNumber::from_u64(12))])
        );
        // Check that the version assignment for each transaction is correct.
        // For a transaction that uses the shared object with mutable=false, it won't update the version
        // using lamport version, hence the next transaction will use the same version number.
        // In the following case, certs[2] has the same assignment as certs[1] for this reason.
        assert_eq!(
            assigned_versions.0,
            vec![
                (
                    certs[0].key(),
                    vec![((id, init_shared_version), init_shared_version),]
                ),
                (
                    certs[1].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(4)),]
                ),
                (
                    certs[2].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(4)),]
                ),
                (
                    certs[3].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(10)),]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn test_assign_versions_from_consensus_with_randomness() {
        let authority = TestAuthorityBuilder::new().build().await;
        let epoch_store = authority.epoch_store_for_testing();
        let randomness_obj_version = epoch_store
            .epoch_start_config()
            .randomness_obj_initial_shared_version()
            .unwrap();
        let certs = vec![
            VerifiedExecutableTransaction::new_system(
                VerifiedTransaction::new_randomness_state_update(
                    epoch_store.epoch(),
                    RandomnessRound::new(1),
                    vec![],
                    randomness_obj_version,
                ),
                epoch_store.epoch(),
            ),
            generate_shared_objs_tx_with_gas_version(
                &[(
                    SUI_RANDOMNESS_STATE_OBJECT_ID,
                    randomness_obj_version,
                    // This can only be false since it's not allowed to use randomness object with mutable=true.
                    false,
                )],
                3,
            ),
            generate_shared_objs_tx_with_gas_version(
                &[(
                    SUI_RANDOMNESS_STATE_OBJECT_ID,
                    randomness_obj_version,
                    false,
                )],
                5,
            ),
        ];
        let assignables = certs
            .iter()
            .map(Schedulable::Transaction)
            .collect::<Vec<_>>();
        let ConsensusSharedObjVerAssignment {
            shared_input_next_versions,
            assigned_versions,
        } = SharedObjVerManager::assign_versions_from_consensus(
            &epoch_store,
            authority.get_object_cache_reader().as_ref(),
            assignables.iter(),
            &BTreeMap::new(),
        )
        .unwrap();
        // Check that the randomness object's next version is initialized.
        assert_eq!(
            epoch_store
                .get_next_object_version(&SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version)
                .unwrap(),
            randomness_obj_version
        );
        let next_randomness_obj_version = randomness_obj_version.next();
        assert_eq!(
            shared_input_next_versions,
            // Randomness object's version is only incremented by 1 regardless of lamport version.
            HashMap::from([(
                (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                next_randomness_obj_version
            )])
        );
        assert_eq!(
            assigned_versions.0,
            vec![
                (
                    certs[0].key(),
                    vec![(
                        (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                        randomness_obj_version
                    ),]
                ),
                (
                    certs[1].key(),
                    // It is critical that the randomness object version is updated before the assignment.
                    vec![(
                        (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                        next_randomness_obj_version
                    )]
                ),
                (
                    certs[2].key(),
                    // It is critical that the randomness object version is updated before the assignment.
                    vec![(
                        (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                        next_randomness_obj_version
                    )]
                ),
            ]
        );
    }

    // Tests shared object version assignment for cancelled transaction.
    #[tokio::test]
    async fn test_assign_versions_from_consensus_with_cancellation() {
        let shared_object_1 = Object::shared_for_testing();
        let shared_object_2 = Object::shared_for_testing();
        let id1 = shared_object_1.id();
        let id2 = shared_object_2.id();
        let init_shared_version_1 = shared_object_1.owner.start_version().unwrap();
        let init_shared_version_2 = shared_object_2.owner.start_version().unwrap();
        let authority = TestAuthorityBuilder::new()
            .with_starting_objects(&[shared_object_1.clone(), shared_object_2.clone()])
            .build()
            .await;
        let randomness_obj_version = authority
            .epoch_store_for_testing()
            .epoch_start_config()
            .randomness_obj_initial_shared_version()
            .unwrap();

        // Generate 5 transactions for testing.
        //   tx1: shared_object_1, shared_object_2, owned_object_version = 3
        //   tx2: shared_object_1, shared_object_2, owned_object_version = 5
        //   tx3: shared_object_1, owned_object_version = 1
        //   tx4: shared_object_1, shared_object_2, owned_object_version = 9
        //   tx5: shared_object_1, shared_object_2, owned_object_version = 11
        //
        // Later, we cancel transaction 2 and 4 due to congestion, and 5 due to DKG failure.
        // Expected outcome:
        //   tx1: both shared objects assign version 1, lamport version = 4
        //   tx2: shared objects assign cancelled version, lamport version = 6 due to gas object version = 5
        //   tx3: shared object 1 assign version 4, lamport version = 5
        //   tx4: shared objects assign cancelled version, lamport version = 10 due to gas object version = 9
        //   tx5: shared objects assign cancelled version, lamport version = 12 due to gas object version = 11
        let certs = vec![
            generate_shared_objs_tx_with_gas_version(
                &[
                    (id1, init_shared_version_1, true),
                    (id2, init_shared_version_2, true),
                ],
                3,
            ),
            generate_shared_objs_tx_with_gas_version(
                &[
                    (id1, init_shared_version_1, true),
                    (id2, init_shared_version_2, true),
                ],
                5,
            ),
            generate_shared_objs_tx_with_gas_version(&[(id1, init_shared_version_1, true)], 1),
            generate_shared_objs_tx_with_gas_version(
                &[
                    (id1, init_shared_version_1, true),
                    (id2, init_shared_version_2, true),
                ],
                9,
            ),
            generate_shared_objs_tx_with_gas_version(
                &[
                    (
                        SUI_RANDOMNESS_STATE_OBJECT_ID,
                        randomness_obj_version,
                        false,
                    ),
                    (id2, init_shared_version_2, true),
                ],
                11,
            ),
        ];
        let epoch_store = authority.epoch_store_for_testing();

        // Cancel transactions 2 and 4 due to congestion.
        let cancelled_txns: BTreeMap<TransactionDigest, CancelConsensusCertificateReason> = [
            (
                *certs[1].digest(),
                CancelConsensusCertificateReason::CongestionOnObjects(vec![id1]),
            ),
            (
                *certs[3].digest(),
                CancelConsensusCertificateReason::CongestionOnObjects(vec![id2]),
            ),
            (
                *certs[4].digest(),
                CancelConsensusCertificateReason::DkgFailed,
            ),
        ]
        .into_iter()
        .collect();

        let assignables = certs
            .iter()
            .map(Schedulable::Transaction)
            .collect::<Vec<_>>();

        // Run version assignment logic.
        let ConsensusSharedObjVerAssignment {
            shared_input_next_versions,
            assigned_versions,
        } = SharedObjVerManager::assign_versions_from_consensus(
            &epoch_store,
            authority.get_object_cache_reader().as_ref(),
            assignables.iter(),
            &cancelled_txns,
        )
        .unwrap();

        // Check that the final version of the shared object is the lamport version of the last
        // transaction.
        assert_eq!(
            shared_input_next_versions,
            HashMap::from([
                ((id1, init_shared_version_1), SequenceNumber::from_u64(5)), // determined by tx3
                ((id2, init_shared_version_2), SequenceNumber::from_u64(4)), // determined by tx1
                (
                    (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                    SequenceNumber::from_u64(1)
                ), // not mutable
            ])
        );

        // Check that the version assignment for each transaction is correct.
        assert_eq!(
            assigned_versions.0,
            vec![
                (
                    certs[0].key(),
                    vec![
                        ((id1, init_shared_version_1), init_shared_version_1),
                        ((id2, init_shared_version_2), init_shared_version_2)
                    ]
                ),
                (
                    certs[1].key(),
                    vec![
                        ((id1, init_shared_version_1), SequenceNumber::CONGESTED),
                        ((id2, init_shared_version_2), SequenceNumber::CANCELLED_READ),
                    ]
                ),
                (
                    certs[2].key(),
                    vec![((id1, init_shared_version_1), SequenceNumber::from_u64(4)),]
                ),
                (
                    certs[3].key(),
                    vec![
                        ((id1, init_shared_version_1), SequenceNumber::CANCELLED_READ),
                        ((id2, init_shared_version_2), SequenceNumber::CONGESTED)
                    ]
                ),
                (
                    certs[4].key(),
                    vec![
                        (
                            (SUI_RANDOMNESS_STATE_OBJECT_ID, randomness_obj_version),
                            SequenceNumber::RANDOMNESS_UNAVAILABLE
                        ),
                        ((id2, init_shared_version_2), SequenceNumber::CANCELLED_READ)
                    ]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn test_assign_versions_from_effects() {
        let shared_object = Object::shared_for_testing();
        let id = shared_object.id();
        let init_shared_version = shared_object.owner.start_version().unwrap();
        let authority = TestAuthorityBuilder::new()
            .with_starting_objects(&[shared_object.clone()])
            .build()
            .await;
        let certs = vec![
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 3),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, false)], 5),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 9),
            generate_shared_objs_tx_with_gas_version(&[(id, init_shared_version, true)], 11),
        ];
        let effects = vec![
            TestEffectsBuilder::new(certs[0].data()).build(),
            TestEffectsBuilder::new(certs[1].data())
                .with_shared_input_versions(BTreeMap::from([(id, SequenceNumber::from_u64(4))]))
                .build(),
            TestEffectsBuilder::new(certs[2].data())
                .with_shared_input_versions(BTreeMap::from([(id, SequenceNumber::from_u64(4))]))
                .build(),
            TestEffectsBuilder::new(certs[3].data())
                .with_shared_input_versions(BTreeMap::from([(id, SequenceNumber::from_u64(10))]))
                .build(),
        ];
        let epoch_store = authority.epoch_store_for_testing();
        let assigned_versions = SharedObjVerManager::assign_versions_from_effects(
            certs
                .iter()
                .zip(effects.iter())
                .collect::<Vec<_>>()
                .as_slice(),
            &epoch_store,
            authority.get_object_cache_reader().as_ref(),
        );
        // Check that the shared object's next version is always initialized in the epoch store.
        assert_eq!(
            epoch_store
                .get_next_object_version(&id, init_shared_version)
                .unwrap(),
            init_shared_version
        );
        assert_eq!(
            assigned_versions.0,
            vec![
                (
                    certs[0].key(),
                    vec![((id, init_shared_version), init_shared_version),]
                ),
                (
                    certs[1].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(4)),]
                ),
                (
                    certs[2].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(4)),]
                ),
                (
                    certs[3].key(),
                    vec![((id, init_shared_version), SequenceNumber::from_u64(10)),]
                ),
            ]
        );
    }

    /// Generate a transaction that uses shared objects as specified in the parameters.
    /// Also uses a gas object with specified version.
    /// The version of the gas object is used to manipulate the lamport version of this transaction.
    fn generate_shared_objs_tx_with_gas_version(
        shared_objects: &[(ObjectID, SequenceNumber, bool)],
        gas_object_version: u64,
    ) -> VerifiedExecutableTransaction {
        let mut builder = ProgrammableTransactionBuilder::new();
        for (shared_object_id, shared_object_init_version, shared_object_mutable) in shared_objects
        {
            builder
                .obj(ObjectArg::SharedObject {
                    id: *shared_object_id,
                    initial_shared_version: *shared_object_init_version,
                    mutable: *shared_object_mutable,
                })
                .unwrap();
        }
        let tx_data = TestTransactionBuilder::new(
            SuiAddress::ZERO,
            (
                ObjectID::random(),
                SequenceNumber::from_u64(gas_object_version),
                ObjectDigest::random(),
            ),
            0,
        )
        .programmable(builder.finish())
        .build();
        let tx = SenderSignedData::new(tx_data, vec![]);
        VerifiedExecutableTransaction::new_unchecked(ExecutableTransaction::new_from_data_and_sig(
            tx,
            CertificateProof::new_system(0),
        ))
    }
}
