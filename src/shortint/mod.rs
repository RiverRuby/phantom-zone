use itertools::Itertools;

use crate::{
    bool::keys::{ClientKey, PublicKey},
    Decryptor, Encryptor, MultiPartyDecryptor,
};

mod ops;
mod types;

type FheUint8 = types::FheUint8<Vec<u64>>;

impl Encryptor<u8, FheUint8> for ClientKey {
    fn encrypt(&self, m: &u8) -> FheUint8 {
        let cts = (0..8)
            .into_iter()
            .map(|i| {
                let bit = ((m >> i) & 1) == 1;
                Encryptor::<bool, Vec<u64>>::encrypt(self, &bit)
            })
            .collect_vec();
        FheUint8 { data: cts }
    }
}

impl Decryptor<u8, FheUint8> for ClientKey {
    fn decrypt(&self, c: &FheUint8) -> u8 {
        assert!(c.data.len() == 8);
        let mut out = 0u8;
        c.data().iter().enumerate().for_each(|(index, bit_c)| {
            let bool = Decryptor::<bool, Vec<u64>>::decrypt(self, bit_c);
            if bool {
                out += 1 << index;
            }
        });
        out
    }
}

impl<M, R, Mo> Encryptor<u8, FheUint8> for PublicKey<M, R, Mo>
where
    PublicKey<M, R, Mo>: Encryptor<bool, Vec<u64>>,
{
    fn encrypt(&self, m: &u8) -> FheUint8 {
        let cts = (0..8)
            .into_iter()
            .map(|i| {
                let bit = ((m >> i) & 1) == 1;
                Encryptor::<bool, Vec<u64>>::encrypt(self, &bit)
            })
            .collect_vec();
        FheUint8 { data: cts }
    }
}

impl MultiPartyDecryptor<u8, FheUint8> for ClientKey
where
    ClientKey: MultiPartyDecryptor<bool, Vec<u64>>,
{
    type DecryptionShare = Vec<<Self as MultiPartyDecryptor<bool, Vec<u64>>>::DecryptionShare>;
    fn gen_decryption_share(&self, c: &FheUint8) -> Self::DecryptionShare {
        assert!(c.data().len() == 8);
        c.data()
            .iter()
            .map(|bit_c| {
                let decryption_share =
                    MultiPartyDecryptor::<bool, Vec<u64>>::gen_decryption_share(self, bit_c);
                decryption_share
            })
            .collect_vec()
    }

    fn aggregate_decryption_shares(&self, c: &FheUint8, shares: &[Self::DecryptionShare]) -> u8 {
        let mut out = 0u8;

        (0..8).into_iter().for_each(|i| {
            // Collect bit i^th decryption share of each party
            let bit_i_decryption_shares = shares.iter().map(|s| s[i]).collect_vec();
            let bit_i = MultiPartyDecryptor::<bool, Vec<u64>>::aggregate_decryption_shares(
                self,
                &c.data()[i],
                &bit_i_decryption_shares,
            );

            if bit_i {
                out += 1 << i;
            }
        });

        out
    }
}

mod frontend {
    use super::ops::{
        arbitrary_bit_adder, arbitrary_bit_division_for_quotient_and_rem, arbitrary_bit_subtractor,
        eight_bit_mul,
    };
    use crate::{
        bool::{evaluator::BoolEvaluator, keys::ServerKeyEvaluationDomain},
        utils::{Global, WithLocal},
    };

    use super::FheUint8;

    mod arithetic {
        use crate::bool::{evaluator::BooleanGates, FheBool};

        use super::*;
        use std::ops::{Add, AddAssign, Div, Mul, Rem, Sub};

        impl AddAssign<&FheUint8> for FheUint8 {
            fn add_assign(&mut self, rhs: &FheUint8) {
                BoolEvaluator::with_local_mut_mut(&mut |e| {
                    let key = ServerKeyEvaluationDomain::global();
                    arbitrary_bit_adder(e, self.data_mut(), rhs.data(), false, key);
                });
            }
        }

        impl Add<&FheUint8> for &FheUint8 {
            type Output = FheUint8;
            fn add(self, rhs: &FheUint8) -> Self::Output {
                let mut a = self.clone();
                a += rhs;
                a
            }
        }

        impl Sub<&FheUint8> for &FheUint8 {
            type Output = FheUint8;
            fn sub(self, rhs: &FheUint8) -> Self::Output {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (out, _, _) = arbitrary_bit_subtractor(e, self.data(), rhs.data(), key);
                    FheUint8 { data: out }
                })
            }
        }

        impl Mul<&FheUint8> for &FheUint8 {
            type Output = FheUint8;
            fn mul(self, rhs: &FheUint8) -> Self::Output {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let out = eight_bit_mul(e, self.data(), rhs.data(), key);
                    FheUint8 { data: out }
                })
            }
        }

        impl Div<&FheUint8> for &FheUint8 {
            type Output = FheUint8;
            fn div(self, rhs: &FheUint8) -> Self::Output {
                // TODO(Jay:) Figure out how to set zero error flag
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (quotient, _) = arbitrary_bit_division_for_quotient_and_rem(
                        e,
                        self.data(),
                        rhs.data(),
                        key,
                    );
                    FheUint8 { data: quotient }
                })
            }
        }

        impl Rem<&FheUint8> for &FheUint8 {
            type Output = FheUint8;
            fn rem(self, rhs: &FheUint8) -> Self::Output {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (_, remainder) = arbitrary_bit_division_for_quotient_and_rem(
                        e,
                        self.data(),
                        rhs.data(),
                        key,
                    );
                    FheUint8 { data: remainder }
                })
            }
        }

        impl FheUint8 {
            pub fn overflowing_add_assign(&mut self, rhs: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut_mut(&mut |e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (overflow, _) =
                        arbitrary_bit_adder(e, self.data_mut(), rhs.data(), false, key);
                    overflow
                })
            }

            pub fn overflowing_add(self, rhs: &FheUint8) -> (FheUint8, FheBool) {
                BoolEvaluator::with_local_mut(|e| {
                    let mut lhs = self.clone();
                    let key = ServerKeyEvaluationDomain::global();
                    let (overflow, _) =
                        arbitrary_bit_adder(e, lhs.data_mut(), rhs.data(), false, key);
                    (lhs, overflow)
                })
            }

            pub fn overflowing_sub(&self, rhs: &FheUint8) -> (FheUint8, FheBool) {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (out, mut overflow, _) =
                        arbitrary_bit_subtractor(e, self.data(), rhs.data(), key);
                    e.not_inplace(&mut overflow);
                    (FheUint8 { data: out }, overflow)
                })
            }

            pub fn div_rem(&self, rhs: &FheUint8) -> (FheUint8, FheUint8) {
                // TODO(Jay:) Figure out how to set zero error flag
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let (quotient, remainder) = arbitrary_bit_division_for_quotient_and_rem(
                        e,
                        self.data(),
                        rhs.data(),
                        key,
                    );
                    (FheUint8 { data: quotient }, FheUint8 { data: remainder })
                })
            }
        }
    }

    mod booleans {
        use crate::{
            bool::{evaluator::BooleanGates, FheBool},
            shortint::ops::{
                arbitrary_bit_comparator, arbitrary_bit_equality, arbitrary_signed_bit_comparator,
            },
        };

        use super::*;

        impl FheUint8 {
            /// a == b
            pub fn eq(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    arbitrary_bit_equality(e, self.data(), other.data(), key)
                })
            }

            /// a != b
            pub fn neq(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let mut is_equal = arbitrary_bit_equality(e, self.data(), other.data(), key);
                    e.not_inplace(&mut is_equal);
                    is_equal
                })
            }

            /// a < b
            pub fn lt(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    arbitrary_bit_comparator(e, other.data(), self.data(), key)
                })
            }

            /// a > b
            pub fn gt(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    arbitrary_bit_comparator(e, self.data(), other.data(), key)
                })
            }

            /// a <= b
            pub fn le(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let mut a_greater_b =
                        arbitrary_bit_comparator(e, self.data(), other.data(), key);
                    e.not_inplace(&mut a_greater_b);
                    a_greater_b
                })
            }

            /// a >= b
            pub fn ge(&self, other: &FheUint8) -> FheBool {
                BoolEvaluator::with_local_mut(|e| {
                    let key = ServerKeyEvaluationDomain::global();
                    let mut a_less_b = arbitrary_bit_comparator(e, other.data(), self.data(), key);
                    e.not_inplace(&mut a_less_b);
                    a_less_b
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use num_traits::Euclid;

    use crate::{
        bool::{
            aggregate_public_key_shares, aggregate_server_key_shares, gen_client_key, gen_keys,
            gen_mp_keys_phase1, gen_mp_keys_phase2,
            parameters::{MP_BOOL_PARAMS, SP_BOOL_PARAMS},
            set_mp_seed, set_parameter_set,
        },
        shortint::types::FheUint8,
        Decryptor, Encryptor, MultiPartyDecryptor,
    };

    #[test]
    fn all_uint8_apis() {
        set_parameter_set(&SP_BOOL_PARAMS);

        let (ck, sk) = gen_keys();
        sk.set_server_key();

        for i in 144..=255 {
            for j in 100..=255 {
                let m0 = i;
                let m1 = j;
                let c0 = ck.encrypt(&m0);
                let c1 = ck.encrypt(&m1);

                assert!(ck.decrypt(&c0) == m0);
                assert!(ck.decrypt(&c1) == m1);

                // Arithmetic
                {
                    {
                        // Add
                        let mut c_m0_plus_m1 = FheUint8 {
                            data: c0.data().to_vec(),
                        };
                        c_m0_plus_m1 += &c1;
                        let m0_plus_m1 = ck.decrypt(&c_m0_plus_m1);
                        assert_eq!(
                            m0_plus_m1,
                            m0.wrapping_add(m1),
                            "Expected {} but got {m0_plus_m1} for {i}+{j}",
                            m0.wrapping_add(m1)
                        );
                    }
                    {
                        // Sub
                        let c_sub = &c0 - &c1;
                        let m0_sub_m1 = ck.decrypt(&c_sub);
                        assert_eq!(
                            m0_sub_m1,
                            m0.wrapping_sub(m1),
                            "Expected {} but got {m0_sub_m1} for {i}-{j}",
                            m0.wrapping_sub(m1)
                        );
                    }

                    {
                        // Mul
                        let c_m0m1 = &c0 * &c1;
                        let m0m1 = ck.decrypt(&c_m0m1);
                        assert_eq!(
                            m0m1,
                            m0.wrapping_mul(m1),
                            "Expected {} but got {m0m1} for {i}x{j}",
                            m0.wrapping_mul(m1)
                        );
                    }

                    // Div & Rem
                    {
                        let (c_quotient, c_rem) = c0.div_rem(&c1);
                        let m_quotient = ck.decrypt(&c_quotient);
                        let m_remainder = ck.decrypt(&c_rem);
                        if j != 0 {
                            let (q, r) = i.div_rem_euclid(&j);
                            assert_eq!(
                                m_quotient, q,
                                "Expected {} but got {m_quotient} for {i}/{j}",
                                q
                            );
                            assert_eq!(
                                m_remainder, r,
                                "Expected {} but got {m_quotient} for {i}%{j}",
                                r
                            );
                        } else {
                            assert_eq!(
                                m_quotient, 255,
                                "Expected 255 but got {m_quotient}. Case div by zero"
                            );
                            assert_eq!(
                                m_remainder, i,
                                "Expected {i} but got {m_quotient}. Case div by zero"
                            )
                        }
                    }
                }

                // Comparisons
                {
                    {
                        let c_eq = c0.eq(&c1);
                        let is_eq = ck.decrypt(&c_eq);
                        assert_eq!(
                            is_eq,
                            i == j,
                            "Expected {} but got {is_eq} for {i}=={j}",
                            i == j
                        );
                    }

                    {
                        let c_gt = c0.gt(&c1);
                        let is_gt = ck.decrypt(&c_gt);
                        assert_eq!(
                            is_gt,
                            i > j,
                            "Expected {} but got {is_gt} for {i}>{j}",
                            i > j
                        );
                    }

                    {
                        let c_lt = c0.lt(&c1);
                        let is_lt = ck.decrypt(&c_lt);
                        assert_eq!(
                            is_lt,
                            i < j,
                            "Expected {} but got {is_lt} for {i}<{j}",
                            i < j
                        );
                    }

                    {
                        let c_ge = c0.ge(&c1);
                        let is_ge = ck.decrypt(&c_ge);
                        assert_eq!(
                            is_ge,
                            i >= j,
                            "Expected {} but got {is_ge} for {i}>={j}",
                            i >= j
                        );
                    }

                    {
                        let c_le = c0.le(&c1);
                        let is_le = ck.decrypt(&c_le);
                        assert_eq!(
                            is_le,
                            i <= j,
                            "Expected {} but got {is_le} for {i}<={j}",
                            i <= j
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fheuint8_test_multi_party() {
        set_parameter_set(&MP_BOOL_PARAMS);
        set_mp_seed([0; 32]);

        let parties = 8;

        // client keys and public key share
        let cks = (0..parties)
            .into_iter()
            .map(|i| gen_client_key())
            .collect_vec();

        // round 1: generate pulic key shares
        let pk_shares = cks.iter().map(|key| gen_mp_keys_phase1(key)).collect_vec();

        let public_key = aggregate_public_key_shares(&pk_shares);

        // round 2: generate server key shares
        let server_key_shares = cks
            .iter()
            .map(|key| gen_mp_keys_phase2(key, &public_key))
            .collect_vec();

        // server aggregates the server key
        let server_key = aggregate_server_key_shares(&server_key_shares);
        server_key.set_server_key();

        // Clients use Pk to encrypt private inputs
        let a = 8u8;
        let b = 10u8;
        let c = 155u8;
        let ct_a = public_key.encrypt(&a);
        let ct_b = public_key.encrypt(&b);
        let ct_c = public_key.encrypt(&c);

        // server computes
        // a*b + c
        let mut ct_ab = &ct_a * &ct_b;
        ct_ab += &ct_c;

        // decrypt ab and check
        // generate decryption shares
        let decryption_shares = cks
            .iter()
            .map(|k| k.gen_decryption_share(&ct_ab))
            .collect_vec();

        // aggregate and decryption ab
        let ab_add_c = cks[0].aggregate_decryption_shares(&ct_ab, &decryption_shares);
        assert!(ab_add_c == (a.wrapping_mul(b)).wrapping_add(c));
    }
}