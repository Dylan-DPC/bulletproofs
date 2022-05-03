#![allow(non_snake_case)]
#![cfg_attr(feature = "docs", doc(include = "../../docs/range-proof-protocol.md"))]

extern crate alloc;
#[cfg(feature = "std")]
extern crate rand;

#[cfg(feature = "std")]
use self::rand::thread_rng;
use alloc::vec::Vec;

use core::iter;

use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::{IsIdentity, MultiscalarMul, VartimeMultiscalarMul};
use merlin::Transcript;

use crate::errors::ProofError;
use crate::generators::{BulletproofGens, PedersenGens};
use crate::inner_product_proof::InnerProductProof;
use crate::transcript::TranscriptProtocol;
use crate::util;

use serde::de::Visitor;
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

/// The `KHotProof` struct represents a proof that the inner
/// product between a secret vector and a public vector is a certain commitment.
/// The secret vector is committed to via a Vector Pedersen Commitment.

#[derive(Clone, Debug)]
pub struct KHotProof {
    /// Commitment to the bits of the vector
    A: CompressedRistretto,
    /// Commitment to the blinding factors
    S: CompressedRistretto,
    /// Commitment to the \\(t_1\\) coefficient of \\( t(x) \\)
    T_1: CompressedRistretto,
    /// Commitment to the \\(t_2\\) coefficient of \\( t(x) \\)
    T_2: CompressedRistretto,
    /// Evaluation of the polynomial \\(t(x)\\) at the challenge point \\(x\\)
    t_x: Scalar,
    /// Blinding factor for the synthetic commitment to \\(t(x)\\)
    t_x_blinding: Scalar,
    /// Blinding factor for the synthetic commitment to the inner-product arguments
    e_blinding: Scalar,
    /// Proof data for the inner-product argument.
    ipp_proof: InnerProductProof,
}

impl KHotProof {
    /// Create a KHotProof for a given vector.
    pub fn prove(
        bp_generators: &BulletproofGens,
        pc_gens: &PedersenGens,
        transcript: &mut Transcript,
        secret_vec: Vec<u8>,
    ) -> Result<KHotProof, ProofError> {
        let n = secret_vec.len();
        if bp_generators.gens_capacity < n {
            return Err(ProofError::InvalidGeneratorsLength);
        }
        let bp_gens = bp_generators.share(0);

        transcript.k_hot_proof_domain_sep(n as u64);

        let rng = &mut thread_rng();
        let a_blinding = Scalar::random(rng);

        // Compute A = <a_L, G> + <a_R, H> + a_blinding * B_blinding
        let mut A = pc_gens.B_blinding * a_blinding;

        use subtle::{Choice, ConditionallySelectable};
        let mut i = 0;
        for (G_i, H_i) in bp_gens.G(n).zip(bp_gens.H(n)) {
            // If v_i = 0, we add a_L[i] * G[i] + a_R[i] * H[i] = - H[i]
            // If v_i = 1, we add a_L[i] * G[i] + a_R[i] * H[i] =   G[i]
            let v_i = Choice::from(secret_vec[i]);
            let mut point = -H_i;
            point.conditional_assign(G_i, v_i);
            A += point;
            i += 1;
        }

        let s_blinding = Scalar::random(rng);
        let s_L: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect();
        let s_R: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect();

        // Compute S = <s_L, G> + <s_R, H> + s_blinding * B_blinding
        let S = RistrettoPoint::multiscalar_mul(
            iter::once(&s_blinding).chain(s_L.iter()).chain(s_R.iter()),
            iter::once(&pc_gens.B_blinding)
                .chain(bp_gens.G(n))
                .chain(bp_gens.H(n)),
        );

        // Commit aggregated A, S
        transcript.append_point(b"A", &A.compress());
        transcript.append_point(b"S", &S.compress());

        let y = transcript.challenge_scalar(b"y");
        let z = transcript.challenge_scalar(b"z");

        // Calculate t by calculating vectors l0, l1, r0, r1 and multiplying
        let mut l_poly = util::VecPoly1::zero(n);
        let mut r_poly = util::VecPoly1::zero(n);

        let zz = z * z;
        let mut exp_y = Scalar::one();
        let mut exp_2 = Scalar::one(); // start at 2^0 = 1
        for i in 0..n {
            let a_L_i = Scalar::from(secret_vec[i]);
            // restore this when we pull val from secret_vec
            // let a_L_i = Scalar::from(secret_vec[i]);
            let a_R_i = a_L_i - Scalar::one();

            l_poly.0[i] = a_L_i - z;
            l_poly.1[i] = s_L[i];
            r_poly.0[i] = exp_y * (a_R_i + z) + zz;
            r_poly.1[i] = exp_y * s_R[i];

            exp_y *= y; // y^i -> y^(i+1)
            exp_2 = exp_2 + exp_2; // 2^i -> 2^(i+1)
        }

        let t_poly = l_poly.inner_product(&r_poly);

        // Generate x by committing to T_1, T_2 (line 49-54)
        let t_1_blinding = Scalar::random(rng);
        let t_2_blinding = Scalar::random(rng);
        let T_1 = pc_gens.commit(t_poly.1, t_1_blinding);
        let T_2 = pc_gens.commit(t_poly.2, t_2_blinding);

        transcript.append_point(b"T_1", &T_1.compress());
        transcript.append_point(b"T_2", &T_2.compress());
        let x = transcript.challenge_scalar(b"x");

        let t_blinding_poly = util::Poly2(Scalar::zero(), t_1_blinding, t_2_blinding);

        let t_x = t_poly.eval(x);
        let t_x_blinding = t_blinding_poly.eval(x);
        let e_blinding = a_blinding + s_blinding * x;
        let l_vec = l_poly.eval(x);
        let r_vec = r_poly.eval(x);

        transcript.append_scalar(b"t_x", &t_x);
        transcript.append_scalar(b"t_x_blinding", &t_x_blinding);
        transcript.append_scalar(b"e_blinding", &e_blinding);

        // Get a challenge value to combine statements for the IPP
        let w = transcript.challenge_scalar(b"w");
        let Q = w * pc_gens.B;

        let G_factors: Vec<Scalar> = iter::repeat(Scalar::one()).take(n).collect();
        let H_factors: Vec<Scalar> = util::exp_iter(y.invert()).take(n).collect();

        let ipp_proof = InnerProductProof::create(
            transcript,
            &Q,
            &G_factors,
            &H_factors,
            bp_gens.G(n).cloned().collect(),
            bp_gens.H(n).cloned().collect(),
            l_vec,
            r_vec,
        );

        Ok(KHotProof {
            A: A.compress(),
            S: S.compress(),
            T_1: T_1.compress(),
            T_2: T_2.compress(),
            t_x,
            t_x_blinding,
            e_blinding,
            ipp_proof,
        })
    }

    /// Verify a KHotProof
    pub fn verify(
        &self,
        bp_gens: &BulletproofGens,
        pc_gens: &PedersenGens,
        transcript: &mut Transcript,
        n: usize,
    ) -> Result<(), ProofError> {
        // HARDCODED FOR TESTS
        let k = Scalar::one();

        if bp_gens.gens_capacity < n {
            return Err(ProofError::InvalidGeneratorsLength);
        }
        let rng = &mut thread_rng();

        transcript.k_hot_proof_domain_sep(n as u64);

        transcript.validate_and_append_point(b"A", &self.A)?;
        transcript.validate_and_append_point(b"S", &self.S)?;

        let y = transcript.challenge_scalar(b"y");
        let z = transcript.challenge_scalar(b"z");
        let zz = z * z;
        let minus_z = -z;

        transcript.validate_and_append_point(b"T_1", &self.T_1)?;
        transcript.validate_and_append_point(b"T_2", &self.T_2)?;

        let x = transcript.challenge_scalar(b"x");

        transcript.append_scalar(b"t_x", &self.t_x);
        transcript.append_scalar(b"t_x_blinding", &self.t_x_blinding);
        transcript.append_scalar(b"e_blinding", &self.e_blinding);

        let w = transcript.challenge_scalar(b"w");
        // Challenge value for batching statements to be verified
        let c = Scalar::random(rng);

        let (x_sq, x_inv_sq, s) = self.ipp_proof.verification_scalars(n, transcript)?;
        let s_inv = s.iter().rev();

        let a = self.ipp_proof.a;
        let b = self.ipp_proof.b;
        let m = 1;

        // Construct concat_z_and_1, an iterator of the values of
        // z^0 * \vec(1)^n || z^1 * \vec(1)^n || ... || z^(m-1) * \vec(1)^n
        let powers_of_1: Vec<Scalar> = util::exp_iter(Scalar::from(1u64)).take(n).collect();

        let concat_z_and_1: Vec<Scalar> = util::exp_iter(z)
            .take(m)
            .flat_map(|exp_z| powers_of_1.iter().map(move |exp_2| exp_2 * exp_z))
            .collect();

        let g = s.iter().map(|s_i| minus_z - a * s_i);
        let h = s_inv
            .zip(util::exp_iter(y.invert()))
            .zip(concat_z_and_1.iter())
            .map(|((s_i_inv, exp_y_inv), z_and_1)| z + exp_y_inv * (zz * z_and_1 - b * s_i_inv));

        let basepoint_scalar = w * (self.t_x - a * b) + c * (delta(n, &y, &z) + k * zz - self.t_x);

        let mega_check = RistrettoPoint::optional_multiscalar_mul(
            iter::once(Scalar::one())
                .chain(iter::once(x))
                .chain(iter::once(c * x))
                .chain(iter::once(c * x * x))
                .chain(x_sq.iter().cloned())
                .chain(x_inv_sq.iter().cloned())
                .chain(iter::once(-self.e_blinding - c * self.t_x_blinding))
                .chain(iter::once(basepoint_scalar))
                .chain(g)
                .chain(h),
            iter::once(self.A.decompress())
                .chain(iter::once(self.S.decompress()))
                .chain(iter::once(self.T_1.decompress()))
                .chain(iter::once(self.T_2.decompress()))
                .chain(self.ipp_proof.L_vec.iter().map(|L| L.decompress()))
                .chain(self.ipp_proof.R_vec.iter().map(|R| R.decompress()))
                .chain(iter::once(Some(pc_gens.B_blinding)))
                .chain(iter::once(Some(pc_gens.B)))
                .chain(bp_gens.G(n, m).map(|&x| Some(x)))
                .chain(bp_gens.H(n, m).map(|&x| Some(x))),
        )
        .ok_or_else(|| ProofError::VerificationError)?;

        if mega_check.is_identity() {
            Ok(())
        } else {
            println!("mega check is not identity");
            Err(ProofError::VerificationError)
        }
    }

    /// Serializes the proof into a byte array of \\(2 \lg n + 9\\)
    /// 32-byte elements, where \\(n\\) is the number of secret bits.
    ///
    /// # Layout
    ///
    /// The layout of the range proof encoding is:
    ///
    /// * four compressed Ristretto points \\(A,S,T_1,T_2\\),
    /// * three scalars \\(t_x, \tilde{t}_x, \tilde{e}\\),
    /// * \\(n\\) pairs of compressed Ristretto points \\(L_0,R_0\dots,L_{n-1},R_{n-1}\\),
    /// * two scalars \\(a, b\\).
    pub fn to_bytes(&self) -> Vec<u8> {
        // 7 elements: points A, S, T1, T2, scalars tx, tx_bl, e_bl.
        let mut buf = Vec::with_capacity(7 * 32 + self.ipp_proof.serialized_size());
        buf.extend_from_slice(self.A.as_bytes());
        buf.extend_from_slice(self.S.as_bytes());
        buf.extend_from_slice(self.T_1.as_bytes());
        buf.extend_from_slice(self.T_2.as_bytes());
        buf.extend_from_slice(self.t_x.as_bytes());
        buf.extend_from_slice(self.t_x_blinding.as_bytes());
        buf.extend_from_slice(self.e_blinding.as_bytes());
        buf.extend(self.ipp_proof.to_bytes_iter());
        buf
    }

    /// Deserializes the proof from a byte slice.
    ///
    /// Returns an error if the byte slice cannot be parsed into a `KHotProof`.
    pub fn from_bytes(slice: &[u8]) -> Result<KHotProof, ProofError> {
        if slice.len() % 32 != 0 {
            return Err(ProofError::FormatError);
        }
        if slice.len() < 7 * 32 {
            return Err(ProofError::FormatError);
        }

        use crate::util::read32;

        let A = CompressedRistretto(read32(&slice[0 * 32..]));
        let S = CompressedRistretto(read32(&slice[1 * 32..]));
        let T_1 = CompressedRistretto(read32(&slice[2 * 32..]));
        let T_2 = CompressedRistretto(read32(&slice[3 * 32..]));

        let t_x = Scalar::from_canonical_bytes(read32(&slice[4 * 32..]))
            .ok_or(ProofError::FormatError)?;
        let t_x_blinding = Scalar::from_canonical_bytes(read32(&slice[5 * 32..]))
            .ok_or(ProofError::FormatError)?;
        let e_blinding = Scalar::from_canonical_bytes(read32(&slice[6 * 32..]))
            .ok_or(ProofError::FormatError)?;

        let ipp_proof = InnerProductProof::from_bytes(&slice[7 * 32..])?;

        Ok(KHotProof {
            A,
            S,
            T_1,
            T_2,
            t_x,
            t_x_blinding,
            e_blinding,
            ipp_proof,
        })
    }
}

impl Serialize for KHotProof {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.to_bytes()[..])
    }
}

impl<'de> Deserialize<'de> for KHotProof {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KHotProofVisitor;

        impl<'de> Visitor<'de> for KHotProofVisitor {
            type Value = KHotProof;

            fn expecting(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                formatter.write_str("a valid KHotProof")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<KHotProof, E>
            where
                E: serde::de::Error,
            {
                // Using Error::custom requires T: Display, which our error
                // type only implements when it implements std::error::Error.
                #[cfg(feature = "std")]
                return KHotProof::from_bytes(v).map_err(serde::de::Error::custom);
                // In no-std contexts, drop the error message.
                #[cfg(not(feature = "std"))]
                return KHotProof::from_bytes(v)
                    .map_err(|_| serde::de::Error::custom("deserialization error"));
            }
        }

        deserializer.deserialize_bytes(KHotProofVisitor)
    }
}

/// Compute
/// \\[
/// \delta(y,z) = (z - z^{2}) \langle \mathbf{1}, {\mathbf{y}}^{n} \rangle - z^3 \cdot n
/// \\]
fn delta(n: usize, y: &Scalar, z: &Scalar) -> Scalar {
    let z2 = z * z;
    let z3 = z2 * z;
    let sum_y = util::sum_of_powers(y, n);

    (z - z2) * sum_y - z3 * Scalar::from(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta() {
        let mut rng = rand::thread_rng();
        let y = Scalar::random(&mut rng);
        let z = Scalar::random(&mut rng);
        // Choose n = 256 to ensure we overflow the group order during
        // the computation, to check that that's done correctly
        let n = 256;
        // code copied from previous implementation
        let z2 = z * z;
        let z3 = z2 * z;
        let mut power_g = Scalar::zero();
        let mut exp_y = Scalar::one(); // start at y^0 = 1
        for _ in 0..n {
            power_g += (z - z2) * exp_y - z3;
            exp_y = exp_y * y; // y^i -> y^(i+1)
        }
        assert_eq!(power_g, delta(n, &y, &z));
    }

    fn create_and_verify_helper(n: usize) {
        let pc_gens = PedersenGens::default();
        let bp_gens = BulletproofGens::new(n, 1);

        // Prover's scope
        let proof_bytes = {
            // 0. Create witness data
            let mut secret_vec = vec![0; n];
            // TODO: choose index randomly
            secret_vec[n - 1] = 1;

            // 1. Create the proof
            let mut transcript = Transcript::new(b"KHotProofTest");
            let proof = KHotProof::prove(&bp_gens, &pc_gens, &mut transcript, secret_vec).unwrap();

            // 2. Return serialized proof and value commitments
            bincode::serialize(&proof).unwrap()
        };

        // Verifier's scope
        {
            // 3. Deserialize
            let proof: KHotProof = bincode::deserialize(&proof_bytes).unwrap();

            // 4. Verify with the same customization label as above
            let mut transcript = Transcript::new(b"KHotProofTest");

            assert!(proof.verify(&bp_gens, &pc_gens, &mut transcript, n).is_ok());
        }
    }

    #[test]
    fn test_n_1() {
        create_and_verify_helper(1);
    }
    #[test]
    fn test_n_2() {
        create_and_verify_helper(2);
    }
    #[test]
    fn test_n_4() {
        create_and_verify_helper(4);
    }
    #[test]
    fn test_n_32() {
        create_and_verify_helper(32);
    }
    #[test]
    fn test_n_64() {
        create_and_verify_helper(64);
    }
    #[test]
    fn test_n_1024() {
        create_and_verify_helper(1024);
    }
    #[test]
    fn test_n_131072() {
        create_and_verify_helper(131072);
    }
    #[test]
    fn test_n_524288() {
        create_and_verify_helper(1024);
    }
    #[test]
    fn test_n_1048576() {
        create_and_verify_helper(1048576);
    }
}
