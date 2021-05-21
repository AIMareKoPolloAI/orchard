//! Key structures for Orchard.

use std::convert::TryInto;
use std::mem;

use aes::Aes256;
use fpe::ff1::{BinaryNumeralString, FF1};
use group::GroupEncoding;
use halo2::arithmetic::FieldExt;
use pasta_curves::pallas;
use rand::{CryptoRng, RngCore};
use subtle::CtOption;

use crate::{
    address::Address,
    primitives::redpallas::{self, SpendAuth},
    spec::{
        commit_ivk, diversify_hash, extract_p, ka_orchard, prf_expand, prf_expand_vec, prf_nf,
        to_base, to_scalar, NonIdentityPallasPoint, NonZeroPallasBase, NonZeroPallasScalar,
    },
};

/// A spending key, from which all key material is derived.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
pub struct SpendingKey([u8; 32]);

impl SpendingKey {
    /// Generates a random spending key.
    ///
    /// This is only used when generating dummy notes. Real spending keys should be
    /// derived according to [ZIP 32].
    ///
    /// [ZIP 32]: https://zips.z.cash/zip-0032
    pub(crate) fn random(rng: &mut impl RngCore) -> Self {
        loop {
            let mut bytes = [0; 32];
            rng.fill_bytes(&mut bytes);
            let sk = SpendingKey::from_bytes(bytes);
            if sk.is_some().into() {
                break sk.unwrap();
            }
        }
    }

    /// Constructs an Orchard spending key from uniformly-random bytes.
    ///
    /// Returns `None` if the bytes do not correspond to a valid Orchard spending key.
    pub fn from_bytes(sk: [u8; 32]) -> CtOption<Self> {
        let sk = SpendingKey(sk);
        // If ask = 0, discard this key. We call `derive_inner` rather than
        // `SpendAuthorizingKey::from` here because we only need to know
        // whether ask = 0; the adjustment to potentially negate ask is not
        // needed. Also, `from` would panic on ask = 0.
        let ask = SpendAuthorizingKey::derive_inner(&sk);
        // If ivk = ⊥, discard this key.
        let ivk = KeyAgreementPrivateKey::derive_inner(&(&sk).into());
        CtOption::new(sk, !(ask.ct_is_zero() | ivk.is_none()))
    }
}

/// A spend authorizing key, used to create spend authorization signatures.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug)]
pub struct SpendAuthorizingKey(redpallas::SigningKey<SpendAuth>);

impl SpendAuthorizingKey {
    /// Derives ask from sk. Internal use only, does not enforce all constraints.
    fn derive_inner(sk: &SpendingKey) -> pallas::Scalar {
        to_scalar(prf_expand(&sk.0, &[0x06]))
    }

    /// Creates a spend authorization signature over the given message.
    pub fn sign<R: RngCore + CryptoRng>(
        &self,
        rng: R,
        msg: &[u8],
    ) -> redpallas::Signature<SpendAuth> {
        self.0.sign(rng, msg)
    }
}

impl From<&SpendingKey> for SpendAuthorizingKey {
    fn from(sk: &SpendingKey) -> Self {
        let ask = Self::derive_inner(sk);
        // SpendingKey cannot be constructed such that this assertion would fail.
        assert!(!bool::from(ask.ct_is_zero()));
        // TODO: Add TryFrom<S::Scalar> for SpendAuthorizingKey.
        let ret = SpendAuthorizingKey(ask.to_bytes().try_into().unwrap());
        // If the last bit of repr_P(ak) is 1, negate ask.
        if (<[u8; 32]>::from(SpendValidatingKey::from(&ret).0)[31] >> 7) == 1 {
            SpendAuthorizingKey((-ask).to_bytes().try_into().unwrap())
        } else {
            ret
        }
    }
}

/// A key used to validate spend authorization signatures.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
/// Note that this is $\mathsf{ak}^\mathbb{P}$, which by construction is equivalent to
/// $\mathsf{ak}$ but stored here as a RedPallas verification key.
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
pub struct SpendValidatingKey(redpallas::VerificationKey<SpendAuth>);

impl From<&SpendAuthorizingKey> for SpendValidatingKey {
    fn from(ask: &SpendAuthorizingKey) -> Self {
        SpendValidatingKey((&ask.0).into())
    }
}

impl PartialEq for SpendValidatingKey {
    fn eq(&self, other: &Self) -> bool {
        <[u8; 32]>::from(&self.0).eq(&<[u8; 32]>::from(&other.0))
    }
}

impl SpendValidatingKey {
    /// Randomizes this spend validating key with the given `randomizer`.
    pub fn randomize(&self, randomizer: &pallas::Scalar) -> redpallas::VerificationKey<SpendAuth> {
        self.0.randomize(randomizer)
    }
}

/// A key used to derive [`Nullifier`]s from [`Note`]s.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [`Nullifier`]: crate::note::Nullifier
/// [`Note`]: crate::note::Note
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
pub(crate) struct NullifierDerivingKey(pallas::Base);

impl From<&SpendingKey> for NullifierDerivingKey {
    fn from(sk: &SpendingKey) -> Self {
        NullifierDerivingKey(to_base(prf_expand(&sk.0, &[0x07])))
    }
}

impl NullifierDerivingKey {
    pub(crate) fn prf_nf(&self, rho: pallas::Base) -> pallas::Base {
        prf_nf(self.0, rho)
    }
}

/// The randomness for $\mathsf{Commit}^\mathsf{ivk}$.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
struct CommitIvkRandomness(pallas::Scalar);

impl From<&SpendingKey> for CommitIvkRandomness {
    fn from(sk: &SpendingKey) -> Self {
        CommitIvkRandomness(to_scalar(prf_expand(&sk.0, &[0x08])))
    }
}

/// A key that provides the capability to view incoming and outgoing transactions.
///
/// This key is useful anywhere you need to maintain accurate balance, but do not want the
/// ability to spend funds (such as a view-only wallet).
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
pub struct FullViewingKey {
    ak: SpendValidatingKey,
    nk: NullifierDerivingKey,
    rivk: CommitIvkRandomness,
}

impl From<&SpendingKey> for FullViewingKey {
    fn from(sk: &SpendingKey) -> Self {
        FullViewingKey {
            ak: (&SpendAuthorizingKey::from(sk)).into(),
            nk: sk.into(),
            rivk: sk.into(),
        }
    }
}

impl From<FullViewingKey> for SpendValidatingKey {
    fn from(fvk: FullViewingKey) -> Self {
        fvk.ak
    }
}

impl FullViewingKey {
    pub(crate) fn nk(&self) -> &NullifierDerivingKey {
        &self.nk
    }

    /// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
    ///
    /// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
    fn derive_dk_ovk(&self) -> (DiversifierKey, OutgoingViewingKey) {
        let k = self.rivk.0.to_bytes();
        let b = [(&self.ak.0).into(), self.nk.0.to_bytes()];
        let r = prf_expand_vec(&k, &[&[0x82], &b[0][..], &b[1][..]]);
        (
            DiversifierKey(r[..32].try_into().unwrap()),
            OutgoingViewingKey(r[32..].try_into().unwrap()),
        )
    }

    /// Returns the default payment address for this key.
    pub fn default_address(&self) -> Address {
        IncomingViewingKey::from(self).default_address()
    }

    /// Returns the payment address for this key at the given index.
    pub fn address_at(&self, j: impl Into<DiversifierIndex>) -> Address {
        IncomingViewingKey::from(self).address_at(j)
    }

    /// Returns the payment address for this key corresponding to the given diversifier.
    pub fn address(&self, d: Diversifier) -> Address {
        // Shortcut: we don't need to derive DiversifierKey.
        KeyAgreementPrivateKey::from(self).address(d)
    }
}

/// A key that provides the capability to derive a sequence of diversifiers.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug)]
pub struct DiversifierKey([u8; 32]);

impl From<&FullViewingKey> for DiversifierKey {
    fn from(fvk: &FullViewingKey) -> Self {
        fvk.derive_dk_ovk().0
    }
}

/// The index for a particular diversifier.
#[derive(Clone, Copy, Debug)]
pub struct DiversifierIndex([u8; 11]);

macro_rules! di_from {
    ($n:ident) => {
        impl From<$n> for DiversifierIndex {
            fn from(j: $n) -> Self {
                let mut j_bytes = [0; 11];
                j_bytes[..mem::size_of::<$n>()].copy_from_slice(&j.to_le_bytes());
                DiversifierIndex(j_bytes)
            }
        }
    };
}
di_from!(u32);
di_from!(u64);
di_from!(usize);

impl DiversifierKey {
    /// Returns the diversifier at index 0.
    pub fn default_diversifier(&self) -> Diversifier {
        self.get(0u32)
    }

    /// Returns the diversifier at the given index.
    pub fn get(&self, j: impl Into<DiversifierIndex>) -> Diversifier {
        let ff = FF1::<Aes256>::new(&self.0, 2).expect("valid radix");
        let enc = ff
            .encrypt(&[], &BinaryNumeralString::from_bytes_le(&j.into().0[..]))
            .unwrap();
        Diversifier(enc.to_bytes_le().try_into().unwrap())
    }
}

/// A diversifier that can be used to derive a specific [`Address`] from a
/// [`FullViewingKey`] or [`IncomingViewingKey`].
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Clone, Copy, Debug)]
pub struct Diversifier([u8; 11]);

impl Diversifier {
    /// Returns the byte array corresponding to this diversifier.
    pub fn as_array(&self) -> &[u8; 11] {
        &self.0
    }
}

/// The private key $\mathsf{ivk}$ used in $KA^{Orchard}$, for decrypting incoming notes.
///
/// In Sapling this is what was encoded as an incoming viewing key. For Orchard, we store
/// both this and [`DiversifierKey`] inside [`IncomingViewingKey`] for usability (to
/// enable deriving the default address for an incoming viewing key), while this separate
/// type represents $\mathsf{ivk}$.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
///
/// # Implementation notes
///
/// We store $\mathsf{ivk}$ in memory as a scalar instead of a base, so that we aren't
/// incurring an expensive serialize-and-parse step every time we use it (e.g. for trial
/// decryption of notes). When we actually want to serialize ivk, we're guaranteed to get
/// a valid base field element encoding, because we always construct ivk from an integer
/// in the correct range.
#[derive(Debug)]
struct KeyAgreementPrivateKey(NonZeroPallasScalar);

impl From<&FullViewingKey> for KeyAgreementPrivateKey {
    fn from(fvk: &FullViewingKey) -> Self {
        // KeyAgreementPrivateKey cannot be constructed such that this unwrap would fail.
        let ivk = KeyAgreementPrivateKey::derive_inner(fvk).unwrap();
        KeyAgreementPrivateKey(ivk.into())
    }
}

impl KeyAgreementPrivateKey {
    /// Derives ivk from fvk. Internal use only, does not enforce all constraints.
    fn derive_inner(fvk: &FullViewingKey) -> CtOption<NonZeroPallasBase> {
        let ak = extract_p(&pallas::Point::from_bytes(&(&fvk.ak.0).into()).unwrap());
        commit_ivk(&ak, &fvk.nk.0, &fvk.rivk.0)
    }

    /// Returns the payment address for this key corresponding to the given diversifier.
    fn address(&self, d: Diversifier) -> Address {
        let pk_d = DiversifiedTransmissionKey::derive(self, &d);
        Address::from_parts(d, pk_d)
    }
}

/// A key that provides the capability to detect and decrypt incoming notes from the block
/// chain, without being able to spend the notes or detect when they are spent.
///
/// This key is useful in situations where you only need the capability to detect inbound
/// payments, such as merchant terminals.
///
/// This key is not suitable for use on its own in a wallet, as it cannot maintain
/// accurate balance. You should use a [`FullViewingKey`] instead.
///
/// Defined in [Zcash Protocol Spec § 5.6.4.3: Orchard Raw Incoming Viewing Keys][orchardinviewingkeyencoding].
///
/// [orchardinviewingkeyencoding]: https://zips.z.cash/protocol/nu5.pdf#orchardinviewingkeyencoding
#[derive(Debug)]
pub struct IncomingViewingKey {
    dk: DiversifierKey,
    ivk: KeyAgreementPrivateKey,
}

impl From<&FullViewingKey> for IncomingViewingKey {
    fn from(fvk: &FullViewingKey) -> Self {
        IncomingViewingKey {
            dk: fvk.into(),
            ivk: fvk.into(),
        }
    }
}

impl IncomingViewingKey {
    /// Returns the default payment address for this key.
    pub fn default_address(&self) -> Address {
        self.address(self.dk.default_diversifier())
    }

    /// Returns the payment address for this key at the given index.
    pub fn address_at(&self, j: impl Into<DiversifierIndex>) -> Address {
        self.address(self.dk.get(j))
    }

    /// Returns the payment address for this key corresponding to the given diversifier.
    pub fn address(&self, d: Diversifier) -> Address {
        self.ivk.address(d)
    }
}

/// A key that provides the capability to recover outgoing transaction information from
/// the block chain.
///
/// This key is not suitable for use on its own in a wallet, as it cannot maintain
/// accurate balance. You should use a [`FullViewingKey`] instead.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Debug, Clone)]
pub struct OutgoingViewingKey([u8; 32]);

impl From<&FullViewingKey> for OutgoingViewingKey {
    fn from(fvk: &FullViewingKey) -> Self {
        fvk.derive_dk_ovk().1
    }
}

/// The diversified transmission key for a given payment address.
///
/// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
///
/// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
#[derive(Clone, Copy, Debug)]
pub(crate) struct DiversifiedTransmissionKey(NonIdentityPallasPoint);

impl DiversifiedTransmissionKey {
    /// Defined in [Zcash Protocol Spec § 4.2.3: Orchard Key Components][orchardkeycomponents].
    ///
    /// [orchardkeycomponents]: https://zips.z.cash/protocol/nu5.pdf#orchardkeycomponents
    fn derive(ivk: &KeyAgreementPrivateKey, d: &Diversifier) -> Self {
        let g_d = diversify_hash(&d.as_array());
        DiversifiedTransmissionKey(ka_orchard(&ivk.0, &g_d))
    }

    /// $repr_P(self)$
    pub(crate) fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

/// Generators for property testing.
#[cfg(any(test, feature = "test-dependencies"))]
pub mod testing {
    use proptest::prelude::*;

    use super::SpendingKey;

    prop_compose! {
        /// Generate a uniformly distributed fake note commitment value.
        pub fn arb_spending_key()(
            key in prop::array::uniform32(prop::num::u8::ANY)
                .prop_map(SpendingKey::from_bytes)
                .prop_filter(
                    "Values must correspond to valid Orchard spending keys.",
                    |opt| bool::from(opt.is_some())
                )
        ) -> SpendingKey {
            key.unwrap()
        }
    }
}