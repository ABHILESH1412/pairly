//! The SAS pairing exchange run over a fresh `XX` channel. See [`pairly_crypto::sas`] for why
//! the commit/reveal step is there.

use pairly_crypto::sas::{self, SasCode};
use pairly_proto::packets::{PairCommit, PairConfirm, PairNonce, PairReveal};
use tokio::sync::oneshot;

use crate::channel::Channel;
use crate::{CoreError, Result};

pub(crate) struct SasOutcome {
    pub code: SasCode,
    pub pair_secret: [u8; 32],
}

/// Commit/reveal nonces and derive the code both users compare.
pub(crate) async fn exchange(ch: &mut Channel) -> Result<SasOutcome> {
    let h = ch.info.hash;
    let (ni, nr) = if ch.initiator {
        let commit: PairCommit = ch.reader.recv_body().await?;
        let ni = sas::random_nonce();
        ch.writer.send_body(&PairNonce { nonce: ni }).await?;
        let PairReveal { nonce: nr } = ch.reader.recv_body().await?;
        if !sas::verify_commitment(&commit.commitment, &h, &nr) {
            return Err(CoreError::Violation("SAS commitment mismatch"));
        }
        (ni, nr)
    } else {
        let nr = sas::random_nonce();
        ch.writer
            .send_body(&PairCommit {
                commitment: sas::commitment(&h, &nr),
            })
            .await?;
        let PairNonce { nonce: ni } = ch.reader.recv_body().await?;
        ch.writer.send_body(&PairReveal { nonce: nr }).await?;
        (ni, nr)
    };
    Ok(SasOutcome {
        code: sas::sas_code(&h, &ni, &nr),
        pair_secret: sas::pair_secret(&h, &ni, &nr),
    })
}

/// Wait for the local user's decision and the peer's. `Ok` only if both accepted.
pub(crate) async fn confirm(ch: &mut Channel, local: oneshot::Receiver<bool>) -> Result<()> {
    let Channel { reader, writer, .. } = ch;
    let remote = reader.recv_body::<PairConfirm>();
    tokio::pin!(remote, local);
    let (mut local_ok, mut remote_ok) = (false, false);
    while !(local_ok && remote_ok) {
        tokio::select! {
            decision = &mut local, if !local_ok => {
                // A dropped sender (superseded request, shutdown) counts as a rejection.
                let accept = decision.unwrap_or(false);
                writer.send_body(&PairConfirm { accept }).await?;
                if !accept {
                    return Err(CoreError::PairingRejected);
                }
                local_ok = true;
            }
            answer = &mut remote, if !remote_ok => {
                if !answer?.accept {
                    return Err(CoreError::PairingRejected);
                }
                remote_ok = true;
            }
        }
    }
    Ok(())
}
