//! The controller's certificate authority + operational fabric.
//!
//! A persistent controller needs a **stable** identity: the same fabric
//! (RCAC/NOC/IPK/operational key) across restarts so CASE re-address keeps
//! working, *and* the CA **signing key** so it can still mint NOCs for *new*
//! devices after a restart.
//!
//! Division of persistence:
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
    CanonAeadKey, CanonPkcSecretKey, Crypto, Rng as _, SecretKey as _, SigningSecretKey as _,
};
use rs_matter::error::Error;
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;

use crate::matter::FabricIdentity;

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

    /// Startup entry point for **persistent** mode: reuse the controller's CA +
    /// fabric if previously persisted, else bootstrap a fresh one and persist it.
    ///
    /// Precondition: `matter.load_persist(store.kv())` has already run (so any
    /// previously-saved fabric is loaded). The decision:
    /// - if a fabric for `identity` (matched by vendor + node id) is present
    ///   *and* a CA signing-key blob is stored, [`Self::reload`] from them;
    /// - otherwise [`Self::bootstrap`] a new CA/fabric and persist both.
    ///
    /// Returns the CA and our controller fabric index.
    pub fn ensure<C: Crypto, S: rs_matter::persist::KvBlobStore>(
        crypto: &C,
        matter: &rs_matter::Matter<'_>,
        store: &crate::persist::ControllerStore<'_, S>,
        identity: FabricIdentity,
        tier: CaTier,
    ) -> Result<(Self, core::num::NonZeroU8), Error> {
        // Look for our already-loaded fabric + its cert chain.
        let existing = matter.with_state(|state| {
            for fabric in state.fabrics.iter() {
                if fabric.vendor_id() == identity.admin_vendor_id
                    && fabric.node_id() == identity.controller_node_id
                {
                    return Some((
                        fabric.fab_idx(),
                        fabric.root_ca().to_vec(),
                        fabric.icac().to_vec(),
                    ));
                }
            }
            None
        });

        if let Some((fab_idx, rcac, icac)) = existing {
            if let Some((stored_tier, signing_key)) = store.load_ca()? {
                log::info!("controller: reusing persisted fabric (idx {fab_idx})");
                return Ok((Self::reload(stored_tier, rcac, icac, signing_key), fab_idx));
            }
            // Fabric present but no CA key — inconsistent persisted state; fall
            // through to a fresh bootstrap (the stale fabric is harmless/ignored).
            log::warn!("controller: fabric present but no CA key; bootstrapping fresh");
        }

        log::info!("controller: bootstrapping a new CA + fabric");
        let (ca, fab_idx) = Self::bootstrap(crypto, matter, identity, tier)?;
        store.save_ca(&ca)?;
        // Persist the freshly-installed fabric by borrowing it in place (`Fabric`
        // is intentionally not `Clone` — it holds secrets), saving within the
        // state lock.
        matter.with_state(|state| store.save_fabric(state.fabrics.fabric(fab_idx)?))?;
        Ok((ca, fab_idx))
    }

    /// Rebuild the CA from persisted state after a restart: the cert chain comes
    /// from the (already `load_persist`-ed) fabric, the signing key from our KV
    /// blob. The signing key stays in a zero-on-drop [`CanonPkcSecretKey`]
    /// throughout — never a plain `[u8; N]`.
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

    /// A reference to the signing key, for persistence. Keeps the secret inside
    /// `CryptoSensitive`; the caller should expose the raw bytes only transiently
    /// (e.g. straight into a write buffer).
    pub fn signing_key(&self) -> rs_matter::crypto::CanonPkcSecretKeyRef<'_> {
        self.signing_key.reference()
    }

    pub fn tier(&self) -> CaTier {
        self.tier
    }

    /// The RCAC's public key (P-256 SubjectPublicKey, 65-byte uncompressed EC
    /// point), parsed from our held RCAC cert. This is the canonical identity of
    /// the *fabric root*: a `FabricDescriptor.root_public_key` on a device equal
    /// to this means that fabric was created by **our** CA — used by the
    /// orphaned-fabric sweep to recognize (and reclaim) fabrics we left behind.
    ///
    /// Returned owned (the cert accessor borrows the TLV) and works for both
    /// tiers and after `reload` (always derived from the cert, never the key).
    pub fn rcac_public_key(&self) -> Result<Vec<u8>, Error> {
        let cert = rs_matter::cert::CertRef::new(rs_matter::tlv::TLVElement::new(&self.rcac));
        Ok(cert.pubkey()?.to_vec())
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
