//! The controller's certificate authority + operational fabric.
//!
//! A persistent controller needs a **stable** identity: the same fabric
//! (RCAC/NOC/IPK/operational key) across restarts so CASE re-address keeps
//! working, *and* the CA **signing key** so it can still mint NOCs for *new*
//! devices after a restart.
//!
//! Division of persistence (see `docs/phase1-persistence.md`):
//! - The **fabric** (operational key, NOC, RCAC, ICAC, IPK) is persisted by
//!   rs-matter itself (`Matter::load_persist` / `FabricPersist`). It is enough to
//!   re-address commissioned devices over CASE.
//! - The **CA signing key** (RCAC priv in direct mode, ICAC priv in ICAC-tier
//!   mode) is *not* part of a `Fabric`, so we persist it ourselves as a KV blob.
//!   It is what `NocGenerator` needs to sign new device NOCs.
//!
//! Two CA tiers are supported, chosen by [`CaTier`]:
//! - [`CaTier::RcacDirect`] — RCAC signs NOCs directly (`[RCAC, NOC]`).
//! - [`CaTier::Icac`] — an ICAC signs NOCs (`[RCAC, ICAC, NOC]`).

extern crate alloc;
use alloc::vec::Vec;

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::crypto::{
    CanonAeadKey, CanonPkcSecretKey, Crypto, RngCore as _, SecretKey as _, SigningSecretKey as _,
};
use rs_matter::error::Error;
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;

use crate::controller::FabricIdentity;

/// Which CA tier to use when generating the controller's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaTier {
    /// RCAC signs device NOCs directly (`[RCAC, NOC]`). Simplest chain.
    RcacDirect,
    /// An intermediate (ICAC) signs device NOCs (`[RCAC, ICAC, NOC]`).
    Icac,
}

/// The controller's CA material: the cert chain plus the **signing** private key
/// (the only secret we persist ourselves; the operational key lives in the
/// persisted `Fabric`).
///
/// Holds owned cert bytes so it can be rebuilt from persisted state and used to
/// construct a [`NocGenerator`] on demand.
pub struct ControllerCa {
    tier: CaTier,
    /// RCAC TLV cert (always present).
    rcac: Vec<u8>,
    /// ICAC TLV cert (empty in [`CaTier::RcacDirect`]).
    icac: Vec<u8>,
    /// The key that signs device NOCs: RCAC priv (direct) or ICAC priv (ICAC).
    signing_key: CanonPkcSecretKey,
}

impl ControllerCa {
    /// Bootstrap a fresh CA + install the controller's operational fabric into
    /// `matter`, returning the CA (whose signing key the caller should persist)
    /// and the installed fabric index.
    ///
    /// This is the *first-run* path. On a later run the fabric comes back via
    /// `Matter::load_persist` and the CA is rebuilt with [`Self::reload`].
    pub fn bootstrap<C: Crypto>(
        crypto: &C,
        matter: &rs_matter::Matter<'_>,
        identity: FabricIdentity,
        tier: CaTier,
    ) -> Result<(Self, core::num::NonZeroU8), Error> {
        // RCAC (self-signed root). Its priv key signs the ICAC (ICAC tier) or the
        // device NOCs directly (RcacDirect).
        let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut rcac_gen = RcacGenerator::new(&mut rcac_buf);
        let (rcac_priv, rcac) = rcac_gen.generate(crypto, identity.fabric_id, VALID_FOREVER)?;
        let rcac = rcac.to_vec();

        // ICAC (optional tier). When present it is the signing key.
        let (icac, signing_key) = match tier {
            CaTier::RcacDirect => (Vec::new(), copy_key(rcac_priv.reference())),
            CaTier::Icac => {
                let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
                let mut icac_gen = IcacGenerator::new(&mut icac_buf);
                let (icac_priv, icac) =
                    icac_gen.generate(crypto, rcac_priv.reference(), &rcac, VALID_FOREVER)?;
                (icac.to_vec(), copy_key(icac_priv.reference()))
            }
        };
        drop(rcac_priv);

        let ca = Self {
            tier,
            rcac,
            icac,
            signing_key,
        };

        // Mint the controller's own operational NOC + install the fabric.
        let fab_idx = ca.install_fabric(crypto, matter, identity)?;

        Ok((ca, fab_idx))
    }

    /// Rebuild the CA from persisted state after a restart: the cert chain comes
    /// from the (already `load_persist`-ed) fabric, the signing key from our KV
    /// blob.
    pub fn reload(
        tier: CaTier,
        rcac: Vec<u8>,
        icac: Vec<u8>,
        signing_key: CanonPkcSecretKey,
    ) -> Self {
        Self {
            tier,
            rcac,
            icac,
            signing_key,
        }
    }

    /// The signing key bytes to persist (32-byte canonical scalar).
    pub fn signing_key_bytes(&self) -> [u8; 32] {
        *self.signing_key.reference().access()
    }

    pub fn tier(&self) -> CaTier {
        self.tier
    }

    /// Build a [`NocGenerator`] backed by this CA's chain + signing key, into the
    /// caller-provided scratch buffer. Used to sign both the controller's own NOC
    /// (at bootstrap) and device NOCs (at every commission).
    pub fn noc_generator<'a>(&'a self, buf: &'a mut [u8]) -> Result<NocGenerator<'a>, Error> {
        NocGenerator::create(self.signing_key.reference(), &self.rcac, &self.icac, buf)
    }

    /// Mint the controller's operational NOC and add the fabric to `matter`.
    fn install_fabric<C: Crypto>(
        &self,
        crypto: &C,
        matter: &rs_matter::Matter<'_>,
        identity: FabricIdentity,
    ) -> Result<core::num::NonZeroU8, Error> {
        // Controller operational keypair + CSR + canonicalized secret.
        let op_key = crypto.generate_secret_key()?;
        let mut csr_buf = [0u8; 256];
        let csr = op_key.csr(&mut csr_buf)?;
        let mut op_key_canon = CanonPkcSecretKey::new();
        op_key.write_canon(&mut op_key_canon)?;

        // Sign the controller's own NOC with the CA. The NocGenerator needs a
        // TLV+ASN1-sized scratch buffer (not just MAX_CERT_TLV_LEN).
        let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut noc_gen = self.noc_generator(&mut noc_buf)?;
        let noc = noc_gen.generate(crypto, csr, identity.controller_node_id, &[], VALID_FOREVER)?;

        // Fabric IPK: 16 random bytes shared across the fabric.
        let mut ipk = CanonAeadKey::new();
        crypto.rand()?.fill_bytes(ipk.access_mut());

        matter.with_state(|state| {
            let fab = state.fabrics.add(
                crypto,
                op_key_canon.reference(),
                &self.rcac,
                noc,
                &self.icac,
                Some(ipk.reference()),
                identity.admin_vendor_id,
                identity.controller_node_id,
            )?;
            Ok(fab.fab_idx())
        })
    }
}

/// Copy a borrowed canonical secret key into an owned one.
fn copy_key(r: rs_matter::crypto::CanonPkcSecretKeyRef<'_>) -> CanonPkcSecretKey {
    CanonPkcSecretKey::new_from_ref(r)
}
