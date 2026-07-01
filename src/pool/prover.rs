//! Process-wide Groth16 provers.
//!
//! Building a prover parses a ~17 MB embedded zkey, so each is constructed once
//! and shared behind an `Arc`. Proving is CPU-bound, synchronous, and `Send`, so
//! callers run it on `tokio::task::spawn_blocking` off the UI thread (the SDK
//! emits no progress hooks — the UI shows an indeterminate "Generating the ZK
//! Proof" state).

use std::sync::{Arc, OnceLock};

use privacy_pools::{
    CommitmentInputs, CommitmentProver, Groth16Proof, WithdrawInputs, WithdrawProver,
};

use super::PoolError;

static WITHDRAW: OnceLock<Arc<WithdrawProver>> = OnceLock::new();
static COMMITMENT: OnceLock<Arc<CommitmentProver>> = OnceLock::new();

/// The shared withdraw prover (state+ASP membership, 8 public signals), built
/// from the bundled artifacts on first use.
pub fn withdraw_prover() -> Result<Arc<WithdrawProver>, PoolError> {
    if let Some(p) = WITHDRAW.get() {
        return Ok(p.clone());
    }
    tracing::debug!("privacy pools: building withdraw prover from bundled artifacts (~17 MB zkey)");
    let built = Arc::new(
        WithdrawProver::bundled()
            .map_err(|e| PoolError::Proof(format!("build withdraw prover: {e}")))?,
    );
    // A concurrent builder may win the race; either Arc is equivalent.
    let _ = WITHDRAW.set(built.clone());
    Ok(WITHDRAW.get().cloned().unwrap_or(built))
}

/// The shared commitment prover (ragequit, 4 public signals).
pub fn commitment_prover() -> Result<Arc<CommitmentProver>, PoolError> {
    if let Some(p) = COMMITMENT.get() {
        return Ok(p.clone());
    }
    tracing::debug!("privacy pools: building commitment prover from bundled artifacts");
    let built = Arc::new(
        CommitmentProver::bundled()
            .map_err(|e| PoolError::Proof(format!("build commitment prover: {e}")))?,
    );
    let _ = COMMITMENT.set(built.clone());
    Ok(COMMITMENT.get().cloned().unwrap_or(built))
}

/// Prove a withdrawal. Blocking + CPU-bound — call inside `spawn_blocking`.
pub fn prove_withdraw(inputs: &WithdrawInputs) -> Result<Groth16Proof, PoolError> {
    let prover = withdraw_prover()?;
    tracing::info!("privacy pools: generating withdraw ZK proof");
    let started = std::time::Instant::now();
    let proof = prover.prove(inputs)?;
    let prove_ms = started.elapsed().as_millis();
    // Cheap sanity check against the bundled vkey before we spend a tx on it.
    if !prover.verify(&proof)? {
        tracing::error!(
            prove_ms,
            "privacy pools: withdraw ZK proof failed local verification"
        );
        return Err(PoolError::Proof(
            "locally-generated proof failed to verify".into(),
        ));
    }
    tracing::info!(
        prove_ms,
        "privacy pools: withdraw ZK proof generated and verified"
    );
    Ok(proof)
}

/// Prove a ragequit (commitment recompute). Blocking — call inside `spawn_blocking`.
pub fn prove_ragequit(inputs: &CommitmentInputs) -> Result<Groth16Proof, PoolError> {
    let prover = commitment_prover()?;
    tracing::info!("privacy pools: generating ragequit ZK proof");
    let started = std::time::Instant::now();
    let proof = prover.prove(inputs)?;
    let prove_ms = started.elapsed().as_millis();
    if !prover.verify(&proof)? {
        tracing::error!(
            prove_ms,
            "privacy pools: ragequit ZK proof failed local verification"
        );
        return Err(PoolError::Proof(
            "locally-generated ragequit proof failed to verify".into(),
        ));
    }
    tracing::info!(
        prove_ms,
        "privacy pools: ragequit ZK proof generated and verified"
    );
    Ok(proof)
}
