// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::authority_client::AuthorityAPI;
use crate::epoch::committee_store::CommitteeStore;
use crate::transaction_driver::{ExecutedData, SubmitTxResponse, WaitForEffectsResponse};
use prometheus::core::GenericCounter;
use prometheus::{
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry, Histogram,
    HistogramVec, IntCounterVec, Registry,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use sui_types::crypto::AuthorityPublicKeyBytes;
use sui_types::digests::TransactionEventsDigest;
use sui_types::effects::{SignedTransactionEffects, TransactionEffectsAPI, TransactionEvents};
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointRequest, CheckpointResponse, CheckpointSequenceNumber,
};
use sui_types::messages_grpc::{
    HandleCertificateRequestV3, HandleCertificateResponseV2, HandleCertificateResponseV3,
    ObjectInfoRequest, ObjectInfoResponse, RawSubmitTxRequest, RawWaitForEffectsRequest,
    SystemStateRequest, TransactionInfoRequest, TransactionStatus, VerifiedObjectInfoResponse,
};
use sui_types::messages_safe_client::PlainTransactionInfoResponse;
use sui_types::object::Object;
use sui_types::sui_system_state::SuiSystemState;
use sui_types::{base_types::*, committee::*, fp_ensure};
use sui_types::{
    error::{SuiError, SuiResult},
    transaction::*,
};
use tap::TapFallible;
use tracing::{debug, error, instrument};

macro_rules! check_error {
    ($address:expr, $cond:expr, $msg:expr) => {
        $cond.tap_err(|err| {
            if err.individual_error_indicates_epoch_change() {
                debug!(?err, authority=?$address, "Not a real client error");
            } else {
                error!(?err, authority=?$address, $msg);
            }
        })
    }
}

#[derive(Clone)]
pub struct SafeClientMetricsBase {
    total_requests_by_address_method: IntCounterVec,
    total_responses_by_address_method: IntCounterVec,
    latency: HistogramVec,
}

impl SafeClientMetricsBase {
    pub fn new(registry: &Registry) -> Self {
        Self {
            total_requests_by_address_method: register_int_counter_vec_with_registry!(
                "safe_client_total_requests_by_address_method",
                "Total requests to validators group by address and method",
                &["address", "method"],
                registry,
            )
            .unwrap(),
            total_responses_by_address_method: register_int_counter_vec_with_registry!(
                "safe_client_total_responses_by_address_method",
                "Total good (OK) responses from validators group by address and method",
                &["address", "method"],
                registry,
            )
            .unwrap(),
            // Address label is removed to reduce high cardinality, can be added back if needed
            latency: register_histogram_vec_with_registry!(
                "safe_client_latency",
                "RPC latency observed by safe client aggregator, group by method",
                &["method"],
                mysten_metrics::COARSE_LATENCY_SEC_BUCKETS.to_vec(),
                registry,
            )
            .unwrap(),
        }
    }
}

/// Prometheus metrics which can be displayed in Grafana, queried and alerted on
#[derive(Clone)]
pub struct SafeClientMetrics {
    total_requests_handle_transaction_info_request: GenericCounter<prometheus::core::AtomicU64>,
    total_ok_responses_handle_transaction_info_request: GenericCounter<prometheus::core::AtomicU64>,
    total_requests_handle_object_info_request: GenericCounter<prometheus::core::AtomicU64>,
    total_ok_responses_handle_object_info_request: GenericCounter<prometheus::core::AtomicU64>,
    handle_transaction_latency: Histogram,
    handle_certificate_latency: Histogram,
    handle_obj_info_latency: Histogram,
    handle_tx_info_latency: Histogram,
}

impl SafeClientMetrics {
    pub fn new(metrics_base: &SafeClientMetricsBase, validator_address: AuthorityName) -> Self {
        let validator_address = validator_address.to_string();

        let total_requests_handle_transaction_info_request = metrics_base
            .total_requests_by_address_method
            .with_label_values(&[&validator_address, "handle_transaction_info_request"]);
        let total_ok_responses_handle_transaction_info_request = metrics_base
            .total_responses_by_address_method
            .with_label_values(&[&validator_address, "handle_transaction_info_request"]);

        let total_requests_handle_object_info_request = metrics_base
            .total_requests_by_address_method
            .with_label_values(&[&validator_address, "handle_object_info_request"]);
        let total_ok_responses_handle_object_info_request = metrics_base
            .total_responses_by_address_method
            .with_label_values(&[&validator_address, "handle_object_info_request"]);

        let handle_transaction_latency = metrics_base
            .latency
            .with_label_values(&["handle_transaction"]);
        let handle_certificate_latency = metrics_base
            .latency
            .with_label_values(&["handle_certificate"]);
        let handle_obj_info_latency = metrics_base
            .latency
            .with_label_values(&["handle_object_info_request"]);
        let handle_tx_info_latency = metrics_base
            .latency
            .with_label_values(&["handle_transaction_info_request"]);

        Self {
            total_requests_handle_transaction_info_request,
            total_ok_responses_handle_transaction_info_request,
            total_requests_handle_object_info_request,
            total_ok_responses_handle_object_info_request,
            handle_transaction_latency,
            handle_certificate_latency,
            handle_obj_info_latency,
            handle_tx_info_latency,
        }
    }

    pub fn new_for_tests(validator_address: AuthorityName) -> Self {
        let registry = Registry::new();
        let metrics_base = SafeClientMetricsBase::new(&registry);
        Self::new(&metrics_base, validator_address)
    }
}

/// See `SafeClientMetrics::new` for description of each metrics.
/// The metrics are per validator client.
#[derive(Clone)]
pub struct SafeClient<C>
where
    C: Clone,
{
    authority_client: C,
    committee_store: Arc<CommitteeStore>,
    address: AuthorityPublicKeyBytes,
    metrics: SafeClientMetrics,
}

impl<C: Clone> SafeClient<C> {
    pub fn new(
        authority_client: C,
        committee_store: Arc<CommitteeStore>,
        address: AuthorityPublicKeyBytes,
        metrics: SafeClientMetrics,
    ) -> Self {
        Self {
            authority_client,
            committee_store,
            address,
            metrics,
        }
    }
}

impl<C: Clone> SafeClient<C> {
    pub fn authority_client(&self) -> &C {
        &self.authority_client
    }

    #[cfg(test)]
    pub fn authority_client_mut(&mut self) -> &mut C {
        &mut self.authority_client
    }

    fn get_committee(&self, epoch_id: &EpochId) -> SuiResult<Arc<Committee>> {
        self.committee_store
            .get_committee(epoch_id)?
            .ok_or(SuiError::MissingCommitteeAtEpoch(*epoch_id))
    }

    fn check_signed_effects_plain(
        &self,
        digest: &TransactionDigest,
        signed_effects: SignedTransactionEffects,
        expected_effects_digest: Option<&TransactionEffectsDigest>,
    ) -> SuiResult<SignedTransactionEffects> {
        // Check it has the right signer
        fp_ensure!(
            signed_effects.auth_sig().authority == self.address,
            SuiError::ByzantineAuthoritySuspicion {
                authority: self.address,
                reason: format!(
                    "Unexpected validator address in the signed effects signature: {:?}",
                    signed_effects.auth_sig().authority
                ),
            }
        );
        // Checks it concerns the right tx
        fp_ensure!(
            signed_effects.data().transaction_digest() == digest,
            SuiError::ByzantineAuthoritySuspicion {
                authority: self.address,
                reason: "Unexpected tx digest in the signed effects".to_string()
            }
        );
        // check that the effects digest is correct.
        if let Some(effects_digest) = expected_effects_digest {
            fp_ensure!(
                signed_effects.digest() == effects_digest,
                SuiError::ByzantineAuthoritySuspicion {
                    authority: self.address,
                    reason: "Effects digest does not match with expected digest".to_string()
                }
            );
        }
        self.get_committee(&signed_effects.epoch())?;
        Ok(signed_effects)
    }

    fn check_transaction_info(
        &self,
        digest: &TransactionDigest,
        transaction: Transaction,
        status: TransactionStatus,
    ) -> SuiResult<PlainTransactionInfoResponse> {
        fp_ensure!(
            digest == transaction.digest(),
            SuiError::ByzantineAuthoritySuspicion {
                authority: self.address,
                reason: "Signed transaction digest does not match with expected digest".to_string()
            }
        );
        match status {
            TransactionStatus::Signed(signed) => {
                self.get_committee(&signed.epoch)?;
                Ok(PlainTransactionInfoResponse::Signed(
                    SignedTransaction::new_from_data_and_sig(transaction.into_data(), signed),
                ))
            }
            TransactionStatus::Executed(cert_opt, effects, events) => {
                let signed_effects = self.check_signed_effects_plain(digest, effects, None)?;
                match cert_opt {
                    Some(cert) => {
                        let committee = self.get_committee(&cert.epoch)?;
                        let ct = CertifiedTransaction::new_from_data_and_sig(
                            transaction.into_data(),
                            cert,
                        );
                        ct.verify_committee_sigs_only(&committee).map_err(|e| {
                            SuiError::FailedToVerifyTxCertWithExecutedEffects {
                                validator_name: self.address,
                                error: e.to_string(),
                            }
                        })?;
                        Ok(PlainTransactionInfoResponse::ExecutedWithCert(
                            ct,
                            signed_effects,
                            events,
                        ))
                    }
                    None => Ok(PlainTransactionInfoResponse::ExecutedWithoutCert(
                        transaction,
                        signed_effects,
                        events,
                    )),
                }
            }
        }
    }

    fn check_object_response(
        &self,
        request: &ObjectInfoRequest,
        response: ObjectInfoResponse,
    ) -> SuiResult<VerifiedObjectInfoResponse> {
        let ObjectInfoResponse {
            object,
            layout: _,
            lock_for_debugging: _,
        } = response;

        fp_ensure!(
            request.object_id == object.id(),
            SuiError::ByzantineAuthoritySuspicion {
                authority: self.address,
                reason: "Object id mismatch in the response".to_string()
            }
        );

        Ok(VerifiedObjectInfoResponse { object })
    }

    pub fn address(&self) -> &AuthorityPublicKeyBytes {
        &self.address
    }
}

impl<C> SafeClient<C>
where
    C: AuthorityAPI + Send + Sync + Clone + 'static,
{
    /// Submit a transaction for certification and execution.
    pub async fn submit_transaction(
        &self,
        request: RawSubmitTxRequest,
        client_addr: Option<SocketAddr>,
    ) -> Result<SubmitTxResponse, SuiError> {
        let _timer = self.metrics.handle_certificate_latency.start_timer();
        let response = self
            .authority_client
            .submit_transaction(request, client_addr)
            .await?;
        response.try_into()
    }

    /// Wait for effects of a transaction that has been submitted to the network
    /// through the `submit_transaction` API.
    pub async fn wait_for_effects(
        &self,
        request: RawWaitForEffectsRequest,
        client_addr: Option<SocketAddr>,
    ) -> Result<WaitForEffectsResponse, SuiError> {
        let _timer = self.metrics.handle_certificate_latency.start_timer();
        let response = self
            .authority_client
            .wait_for_effects(request, client_addr)
            .await?;

        let wait_for_effects_resp = WaitForEffectsResponse::try_from(response)?;

        match &wait_for_effects_resp {
            WaitForEffectsResponse::Executed {
                effects_digest: _,
                details: Some(details),
            } => {
                self.verify_executed_data((**details).clone())?;
            }
            _ => {
                // No additional verification needed for other response types
            }
        };

        Ok(wait_for_effects_resp)
    }

    /// Initiate a new transfer to a Sui or Primary account.
    pub async fn handle_transaction(
        &self,
        transaction: Transaction,
        client_addr: Option<SocketAddr>,
    ) -> Result<PlainTransactionInfoResponse, SuiError> {
        let _timer = self.metrics.handle_transaction_latency.start_timer();
        let digest = *transaction.digest();
        let response = self
            .authority_client
            .handle_transaction(transaction.clone(), client_addr)
            .await?;
        let response = check_error!(
            self.address,
            self.check_transaction_info(&digest, transaction, response.status),
            "Client error in handle_transaction"
        )?;
        Ok(response)
    }

    fn verify_certificate_response_v2(
        &self,
        digest: &TransactionDigest,
        response: HandleCertificateResponseV2,
    ) -> SuiResult<HandleCertificateResponseV2> {
        let signed_effects =
            self.check_signed_effects_plain(digest, response.signed_effects, None)?;

        Ok(HandleCertificateResponseV2 {
            signed_effects,
            events: response.events,
            fastpath_input_objects: vec![], // unused field
        })
    }

    /// Execute a certificate.
    pub async fn handle_certificate_v2(
        &self,
        certificate: CertifiedTransaction,
        client_addr: Option<SocketAddr>,
    ) -> Result<HandleCertificateResponseV2, SuiError> {
        let digest = *certificate.digest();
        let _timer = self.metrics.handle_certificate_latency.start_timer();
        let response = self
            .authority_client
            .handle_certificate_v2(certificate, client_addr)
            .await?;

        let verified = check_error!(
            self.address,
            self.verify_certificate_response_v2(&digest, response),
            "Client error in handle_certificate"
        )?;
        Ok(verified)
    }

    fn verify_events(
        &self,
        events: &Option<TransactionEvents>,
        events_digest: Option<&TransactionEventsDigest>,
    ) -> SuiResult {
        match (events, events_digest) {
            (None, None) | (None, Some(_)) => Ok(()),
            (Some(events), None) => {
                if !events.data.is_empty() {
                    Err(SuiError::ByzantineAuthoritySuspicion {
                        authority: self.address,
                        reason: "Returned events but no event digest present in effects"
                            .to_string(),
                    })
                } else {
                    Ok(())
                }
            }
            (Some(events), Some(events_digest)) => {
                fp_ensure!(
                    &events.digest() == events_digest,
                    SuiError::ByzantineAuthoritySuspicion {
                        authority: self.address,
                        reason: "Returned events don't match events digest in effects".to_string(),
                    }
                );
                Ok(())
            }
        }
    }

    fn verify_objects<I>(&self, objects: &Option<Vec<Object>>, expected_refs: I) -> SuiResult
    where
        I: IntoIterator<Item = (ObjectID, ObjectRef)>,
    {
        if let Some(objects) = objects {
            let expected: HashMap<_, _> = expected_refs.into_iter().collect();

            for object in objects {
                let object_ref = object.compute_object_reference();
                if expected
                    .get(&object_ref.0)
                    .is_none_or(|expect| &object_ref != expect)
                {
                    return Err(SuiError::ByzantineAuthoritySuspicion {
                        authority: self.address,
                        reason: "Returned object that wasn't present in effects".to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn verify_certificate_response_v3(
        &self,
        digest: &TransactionDigest,
        HandleCertificateResponseV3 {
            effects,
            events,
            input_objects,
            output_objects,
            auxiliary_data,
        }: HandleCertificateResponseV3,
    ) -> SuiResult<HandleCertificateResponseV3> {
        let effects = self.check_signed_effects_plain(digest, effects, None)?;

        // Check Events
        self.verify_events(&events, effects.events_digest())?;

        // Check Input Objects
        self.verify_objects(
            &input_objects,
            effects
                .old_object_metadata()
                .into_iter()
                .map(|(object_ref, _owner)| (object_ref.0, object_ref)),
        )?;

        // Check Output Objects
        self.verify_objects(
            &output_objects,
            effects
                .all_changed_objects()
                .into_iter()
                .map(|(object_ref, _, _)| (object_ref.0, object_ref)),
        )?;

        Ok(HandleCertificateResponseV3 {
            effects,
            events,
            input_objects,
            output_objects,
            auxiliary_data,
        })
    }

    fn verify_executed_data(
        &self,
        ExecutedData {
            effects,
            events,
            input_objects,
            output_objects,
        }: ExecutedData,
    ) -> SuiResult<()> {
        // Check Events
        self.verify_events(&events, effects.events_digest())?;

        // Check Input Objects
        self.verify_objects(
            &Some(input_objects).filter(|v| !v.is_empty()),
            effects
                .old_object_metadata()
                .into_iter()
                .map(|(object_ref, _owner)| (object_ref.0, object_ref)),
        )?;

        // Check Output Objects
        self.verify_objects(
            &Some(output_objects).filter(|v| !v.is_empty()),
            effects
                .all_changed_objects()
                .into_iter()
                .map(|(object_ref, _, _)| (object_ref.0, object_ref)),
        )?;

        Ok(())
    }

    /// Execute a certificate.
    pub async fn handle_certificate_v3(
        &self,
        request: HandleCertificateRequestV3,
        client_addr: Option<SocketAddr>,
    ) -> Result<HandleCertificateResponseV3, SuiError> {
        let digest = *request.certificate.digest();
        let _timer = self.metrics.handle_certificate_latency.start_timer();
        let response = self
            .authority_client
            .handle_certificate_v3(request, client_addr)
            .await?;

        let verified = check_error!(
            self.address,
            self.verify_certificate_response_v3(&digest, response),
            "Client error in handle_certificate"
        )?;
        Ok(verified)
    }

    pub async fn handle_object_info_request(
        &self,
        request: ObjectInfoRequest,
    ) -> Result<VerifiedObjectInfoResponse, SuiError> {
        self.metrics.total_requests_handle_object_info_request.inc();

        let _timer = self.metrics.handle_obj_info_latency.start_timer();
        let response = self
            .authority_client
            .handle_object_info_request(request.clone())
            .await?;
        let response = self
            .check_object_response(&request, response)
            .tap_err(|err| error!(?err, authority=?self.address, "Client error in handle_object_info_request"))?;

        self.metrics
            .total_ok_responses_handle_object_info_request
            .inc();
        Ok(response)
    }

    /// Handle Transaction information requests for a given digest.
    #[instrument(level = "trace", skip_all, fields(authority = ?self.address.concise()))]
    pub async fn handle_transaction_info_request(
        &self,
        request: TransactionInfoRequest,
    ) -> Result<PlainTransactionInfoResponse, SuiError> {
        self.metrics
            .total_requests_handle_transaction_info_request
            .inc();

        let _timer = self.metrics.handle_tx_info_latency.start_timer();

        let transaction_info = self
            .authority_client
            .handle_transaction_info_request(request.clone())
            .await?;

        let transaction = Transaction::new(transaction_info.transaction);
        let transaction_info = self.check_transaction_info(
            &request.transaction_digest,
            transaction,
            transaction_info.status,
        ).tap_err(|err| {
            error!(?err, authority=?self.address, "Client error in handle_transaction_info_request");
        })?;
        self.metrics
            .total_ok_responses_handle_transaction_info_request
            .inc();
        Ok(transaction_info)
    }

    fn verify_checkpoint_sequence(
        &self,
        expected_seq: Option<CheckpointSequenceNumber>,
        checkpoint: &Option<CertifiedCheckpointSummary>,
    ) -> SuiResult {
        let observed_seq = checkpoint.as_ref().map(|c| c.sequence_number);

        if let (Some(e), Some(o)) = (expected_seq, observed_seq) {
            fp_ensure!(
                e == o,
                SuiError::from("Expected checkpoint number doesn't match with returned")
            );
        }
        Ok(())
    }

    fn verify_contents_exist<T, O>(
        &self,
        request_content: bool,
        checkpoint: &Option<T>,
        contents: &Option<O>,
    ) -> SuiResult {
        match (request_content, checkpoint, contents) {
            // If content is requested, checkpoint is not None, but we are not getting any content,
            // it's an error.
            // If content is not requested, or checkpoint is None, yet we are still getting content,
            // it's an error.
            (true, Some(_), None) | (false, _, Some(_)) | (_, None, Some(_)) => Err(
                SuiError::from("Checkpoint contents inconsistent with request"),
            ),
            _ => Ok(()),
        }
    }

    fn verify_checkpoint_response(
        &self,
        request: &CheckpointRequest,
        response: &CheckpointResponse,
    ) -> SuiResult {
        // Verify response data was correct for request
        let CheckpointResponse {
            checkpoint,
            contents,
        } = &response;
        // Checks that the sequence number is correct.
        self.verify_checkpoint_sequence(request.sequence_number, checkpoint)?;
        self.verify_contents_exist(request.request_content, checkpoint, contents)?;
        // Verify signature.
        match checkpoint {
            Some(c) => {
                let epoch_id = c.epoch;
                c.verify_with_contents(&*self.get_committee(&epoch_id)?, contents.as_ref())
            }
            None => Ok(()),
        }
    }

    #[instrument(level = "trace", skip_all, fields(authority = ?self.address.concise()))]
    pub async fn handle_checkpoint(
        &self,
        request: CheckpointRequest,
    ) -> Result<CheckpointResponse, SuiError> {
        let resp = self
            .authority_client
            .handle_checkpoint(request.clone())
            .await?;
        self.verify_checkpoint_response(&request, &resp)
            .tap_err(|err| {
                error!(?err, authority=?self.address, "Client error in handle_checkpoint");
            })?;
        Ok(resp)
    }

    #[instrument(level = "trace", skip_all, fields(authority = ?self.address.concise()))]
    pub async fn handle_system_state_object(&self) -> Result<SuiSystemState, SuiError> {
        self.authority_client
            .handle_system_state_object(SystemStateRequest { _unused: false })
            .await
    }
}
