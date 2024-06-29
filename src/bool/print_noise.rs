use std::{fmt::Debug, iter::Sum};

use itertools::izip;
use num_traits::{FromPrimitive, PrimInt, Zero};
use rand_distr::uniform::SampleUniform;

use crate::{
    backend::{GetModulus, Modulus},
    decomposer::RlweDecomposer,
    lwe::{decrypt_lwe, lwe_key_switch},
    parameters::{BoolParameters, CiphertextModulus},
    random::{DefaultSecureRng, RandomFillUniformInModulus},
    rgsw::{
        decrypt_rlwe, rlwe_auto, rlwe_auto_scratch_rows, RlweCiphertextMutRef, RlweKskRef,
        RuntimeScratchMutRef,
    },
    utils::{encode_x_pow_si_with_emebedding_factor, tests::Stats, TryConvertFrom1},
    ArithmeticOps, ClientKey, Decomposer, MatrixEntity, MatrixMut, ModInit, Ntt, NttInit,
    RowEntity, RowMut, VectorOps,
};

use super::keys::tests::{ideal_sk_lwe, ideal_sk_rlwe};

pub(crate) trait CollectRuntimeServerKeyStats {
    type M;
    /// RGSW ciphertext X^{s[s_index]} in evaluation domain where s the LWE
    /// secret
    fn rgsw_cts_lwe_si(&self, s_index: usize) -> &Self::M;
    /// Auto key in evaluation domain for automorphism g^k. For auto key for
    /// automorphism corresponding to -g, set k = 0
    fn galois_key_for_auto(&self, k: usize) -> &Self::M;
    /// LWE key switching key
    fn lwe_ksk(&self) -> &Self::M;
}

struct ServerKeyStats<T> {
    brk_rgsw_cts: (Stats<T>, Stats<T>),
    post_1_auto: Stats<T>,
    post_lwe_key_switch: Stats<T>,
}

impl<T: PrimInt + FromPrimitive + Debug + Sum> ServerKeyStats<T>
where
    T: for<'a> Sum<&'a T>,
{
    fn new() -> Self {
        ServerKeyStats {
            brk_rgsw_cts: (Stats::default(), Stats::default()),
            post_1_auto: Stats::default(),
            post_lwe_key_switch: Stats::default(),
        }
    }

    fn add_noise_brk_rgsw_cts_nsm(&mut self, noise: &[T]) {
        self.brk_rgsw_cts.0.add_more(noise);
    }

    fn add_noise_brk_rgsw_cts_m(&mut self, noise: &[T]) {
        self.brk_rgsw_cts.1.add_more(noise);
    }

    fn add_noise_post_1_auto(&mut self, noise: &[T]) {
        self.post_1_auto.add_more(&noise);
    }

    fn add_noise_post_kwe_key_switch(&mut self, noise: &[T]) {
        self.post_lwe_key_switch.add_more(&noise);
    }
}

fn collect_server_key_stats<
    M: MatrixEntity + MatrixMut,
    D: Decomposer<Element = M::MatElement>,
    NttOp: NttInit<CiphertextModulus<M::MatElement>> + Ntt<Element = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement>
        + ArithmeticOps<Element = M::MatElement>
        + ModInit<M = CiphertextModulus<M::MatElement>>
        + GetModulus<M = CiphertextModulus<M::MatElement>, Element = M::MatElement>,
    S: CollectRuntimeServerKeyStats<M = M>,
>(
    parameters: BoolParameters<M::MatElement>,
    client_keys: &[ClientKey],
    server_key: &S,
) -> ServerKeyStats<i64>
where
    M::R: RowMut + RowEntity + TryConvertFrom1<[i32], CiphertextModulus<M::MatElement>> + Clone,
    M::MatElement: Copy + PrimInt + FromPrimitive + SampleUniform + Zero + Debug,
{
    let ideal_sk_rlwe = ideal_sk_rlwe(client_keys);
    let ideal_sk_lwe = ideal_sk_lwe(client_keys);

    let embedding_factor = (2 * parameters.rlwe_n().0) / parameters.br_q();
    let rlwe_n = parameters.rlwe_n().0;
    let rlwe_q = parameters.rlwe_q();
    let lwe_q = parameters.lwe_q();
    let rlwe_modop = ModOp::new(rlwe_q.clone());
    let rlwe_nttop = NttOp::new(rlwe_q, rlwe_n);
    let lwe_modop = ModOp::new(*parameters.lwe_q());

    let rlwe_x_rgsw_decomposer = parameters.rlwe_rgsw_decomposer::<D>();
    let (rlwe_x_rgsw_gadget_a, rlwe_x_rgsw_gadget_b) = (
        rlwe_x_rgsw_decomposer.a().gadget_vector(),
        rlwe_x_rgsw_decomposer.b().gadget_vector(),
    );

    let lwe_ks_decomposer = parameters.lwe_decomposer::<D>();

    let mut server_key_stats = ServerKeyStats::new();

    let mut rng = DefaultSecureRng::new();

    // RGSW ciphertext noise
    // Check noise in RGSW ciphertexts of ideal LWE secret elements
    {
        ideal_sk_lwe.iter().enumerate().for_each(|(s_index, s_i)| {
            let rgsw_ct_i = server_key.rgsw_cts_lwe_si(s_index);

            // X^{s[i]}
            let m_si = encode_x_pow_si_with_emebedding_factor::<M::R, _>(
                *s_i,
                embedding_factor,
                rlwe_n,
                rlwe_q,
            );

            // RLWE'(-sm)
            let mut neg_s_eval = M::R::try_convert_from(ideal_sk_rlwe.as_slice(), rlwe_q);
            rlwe_modop.elwise_neg_mut(neg_s_eval.as_mut());
            rlwe_nttop.forward(neg_s_eval.as_mut());

            for j in 0..rlwe_x_rgsw_decomposer.a().decomposition_count() {
                // RLWE(B^{j} * -s[X]*X^{s_lwe[i]})

                // -s[X]*X^{s_lwe[i]}*B_j
                let mut m_ideal = m_si.clone();
                rlwe_nttop.forward(m_ideal.as_mut());
                rlwe_modop.elwise_mul_mut(m_ideal.as_mut(), neg_s_eval.as_ref());
                rlwe_nttop.backward(m_ideal.as_mut());
                rlwe_modop.elwise_scalar_mul_mut(m_ideal.as_mut(), &rlwe_x_rgsw_gadget_a[j]);

                // RLWE(-s*X^{s_lwe[i]}*B_j)
                let mut rlwe_ct = M::zeros(2, rlwe_n);
                rlwe_ct
                    .get_row_mut(0)
                    .copy_from_slice(rgsw_ct_i.get_row_slice(j));
                rlwe_ct.get_row_mut(1).copy_from_slice(
                    rgsw_ct_i.get_row_slice(j + rlwe_x_rgsw_decomposer.a().decomposition_count()),
                );
                // RGSW ciphertexts are in eval domain. We put RLWE ciphertexts back in
                // coefficient domain
                rlwe_ct
                    .iter_rows_mut()
                    .for_each(|r| rlwe_nttop.backward(r.as_mut()));

                let mut m_back = M::R::zeros(rlwe_n);
                decrypt_rlwe(
                    &rlwe_ct,
                    &ideal_sk_rlwe,
                    &mut m_back,
                    &rlwe_nttop,
                    &rlwe_modop,
                );

                // diff
                rlwe_modop.elwise_sub_mut(m_back.as_mut(), m_ideal.as_ref());
                server_key_stats.add_noise_brk_rgsw_cts_nsm(&Vec::<i64>::try_convert_from(
                    m_back.as_ref(),
                    rlwe_q,
                ));
            }

            // RLWE'(m)
            for j in 0..rlwe_x_rgsw_decomposer.b().decomposition_count() {
                // RLWE(B^{j} * X^{s_lwe[i]})

                // X^{s_lwe[i]}*B_j
                let mut m_ideal = m_si.clone();
                rlwe_modop.elwise_scalar_mul_mut(m_ideal.as_mut(), &rlwe_x_rgsw_gadget_b[j]);

                // RLWE(X^{s_lwe[i]}*B_j)
                let mut rlwe_ct = M::zeros(2, rlwe_n);
                rlwe_ct.get_row_mut(0).copy_from_slice(
                    rgsw_ct_i
                        .get_row_slice(j + (2 * rlwe_x_rgsw_decomposer.a().decomposition_count())),
                );
                rlwe_ct
                    .get_row_mut(1)
                    .copy_from_slice(rgsw_ct_i.get_row_slice(
                        j + (2 * rlwe_x_rgsw_decomposer.a().decomposition_count())
                            + rlwe_x_rgsw_decomposer.b().decomposition_count(),
                    ));
                rlwe_ct
                    .iter_rows_mut()
                    .for_each(|r| rlwe_nttop.backward(r.as_mut()));

                let mut m_back = M::R::zeros(rlwe_n);
                decrypt_rlwe(
                    &rlwe_ct,
                    &ideal_sk_rlwe,
                    &mut m_back,
                    &rlwe_nttop,
                    &rlwe_modop,
                );

                // diff
                rlwe_modop.elwise_sub_mut(m_back.as_mut(), m_ideal.as_ref());
                server_key_stats.add_noise_brk_rgsw_cts_m(&Vec::<i64>::try_convert_from(
                    m_back.as_ref(),
                    rlwe_q,
                ));
            }
        });
    }

    // Noise in ciphertext after 1 auto
    // For each auto key g^k. Sample random polynomial m(X) and multiply with
    // -s(X^{g^k}) using key corresponding to auto g^k. Then check the noise in
    // resutling RLWE(m(X) * -s(X^{g^k}))
    {
        let neg_s = {
            let mut s = M::R::try_convert_from(ideal_sk_rlwe.as_slice(), rlwe_q);
            rlwe_modop.elwise_neg_mut(s.as_mut());
            s
        };
        let g = parameters.g();
        let br_q = parameters.br_q();
        let g_dlogs = parameters.auto_element_dlogs();
        let auto_decomposer = parameters.auto_decomposer::<D>();
        let mut scratch_matrix = M::zeros(rlwe_auto_scratch_rows(&auto_decomposer), rlwe_n);
        let mut scratch_matrix_ref = RuntimeScratchMutRef::new(scratch_matrix.as_mut());

        g_dlogs.iter().for_each(|k| {
            let g_pow_k = if *k == 0 {
                -(g as isize)
            } else {
                (g.pow(*k as u32) % br_q) as isize
            };

            // Send s(X) -> s(X^{g^k})
            let (auto_index_map, auto_sign_map) = crate::rgsw::generate_auto_map(rlwe_n, g_pow_k);
            let mut neg_s_g_k = M::R::zeros(rlwe_n);
            izip!(
                neg_s.as_ref().iter(),
                auto_index_map.iter(),
                auto_sign_map.iter()
            )
            .for_each(|(el, to_index, to_sign)| {
                if !to_sign {
                    neg_s_g_k.as_mut()[*to_index] = rlwe_modop.neg(el);
                } else {
                    neg_s_g_k.as_mut()[*to_index] = *el;
                }
            });

            let mut m = M::R::zeros(rlwe_n);
            RandomFillUniformInModulus::random_fill(&mut rng, rlwe_q, m.as_mut());

            // We want -m(X^{g^k})s(X^{g^k}) after key switch
            let want_m = {
                let mut m_g_k_eval = M::R::zeros(rlwe_n);
                // send m(X) -> m(X^{g^k})
                izip!(
                    m.as_ref().iter(),
                    auto_index_map.iter(),
                    auto_sign_map.iter()
                )
                .for_each(|(el, to_index, to_sign)| {
                    if !to_sign {
                        m_g_k_eval.as_mut()[*to_index] = rlwe_modop.neg(el);
                    } else {
                        m_g_k_eval.as_mut()[*to_index] = *el;
                    }
                });

                rlwe_nttop.forward(m_g_k_eval.as_mut());
                let mut s_g_k = neg_s_g_k.clone();
                rlwe_nttop.forward(s_g_k.as_mut());
                rlwe_modop.elwise_mul_mut(m_g_k_eval.as_mut(), s_g_k.as_ref());
                rlwe_nttop.backward(m_g_k_eval.as_mut());
                m_g_k_eval
            };

            // RLWE auto sends part A, A(X), of RLWE to A(X^{g^k}) and then multiplies it
            // with -s(X^{g^k}) using auto key. Deliberately set RLWE = (0, m(X))
            // (ie. m in part A) to get back RLWE(-m(X^{g^k})s(X^{g^k}))
            let mut rlwe = M::zeros(2, rlwe_n);
            rlwe.get_row_mut(0).copy_from_slice(m.as_ref());

            rlwe_auto(
                &mut RlweCiphertextMutRef::new(rlwe.as_mut()),
                &RlweKskRef::new(
                    server_key.galois_key_for_auto(*k).as_ref(),
                    auto_decomposer.decomposition_count(),
                ),
                &mut scratch_matrix_ref,
                &auto_index_map,
                &auto_sign_map,
                &rlwe_modop,
                &rlwe_nttop,
                &auto_decomposer,
                false,
            );

            // decrypt RLWE(-m(X)s(X^{g^k]}))
            let mut back_m = M::R::zeros(rlwe_n);
            decrypt_rlwe(&rlwe, &ideal_sk_rlwe, &mut back_m, &rlwe_nttop, &rlwe_modop);

            // check difference
            let mut diff = back_m;
            rlwe_modop.elwise_sub_mut(diff.as_mut(), want_m.as_ref());
            server_key_stats
                .add_noise_post_1_auto(&Vec::<i64>::try_convert_from(diff.as_ref(), rlwe_q));
        });

        // sample random m

        // key switch
    }

    // LWE Key switch
    // LWE key switches LWE_in = LWE_{Q_ks,N, s}(m) = (b, a_0, ... a_N) -> LWE_out =
    // LWE_{Q_{ks}, n, z}(m) = (b', a'_0, ..., a'n)
    // If LWE_in = (0, a = {a_0, ..., a_N}), then LWE_out = LWE(-a \cdot s_{rlwe})
    for _ in 0..10 {
        let mut lwe_in = M::R::zeros(rlwe_n + 1);
        RandomFillUniformInModulus::random_fill(&mut rng, lwe_q, &mut lwe_in.as_mut()[1..]);

        // Key switch
        let mut lwe_out = M::R::zeros(parameters.lwe_n().0 + 1);
        lwe_key_switch(
            &mut lwe_out,
            &lwe_in,
            server_key.lwe_ksk(),
            &lwe_modop,
            &lwe_ks_decomposer,
        );

        // -a \cdot s
        let mut want_m = M::MatElement::zero();
        izip!(lwe_in.as_ref().iter().skip(1), ideal_sk_rlwe.iter()).for_each(|(a, b)| {
            want_m = lwe_modop.add(
                &want_m,
                &lwe_modop.mul(a, &lwe_q.map_element_from_i64(*b as i64)),
            );
        });
        want_m = lwe_modop.neg(&want_m);

        // decrypt lwe out
        let back_m = decrypt_lwe(&lwe_out, &ideal_sk_lwe, &lwe_modop);

        let noise = lwe_modop.sub(&want_m, &back_m);
        server_key_stats.add_noise_post_kwe_key_switch(&vec![lwe_q.map_element_to_i64(&noise)]);
    }

    server_key_stats
    // Auto keys noise

    // Ksk noise
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::collect_server_key_stats;

    #[test]
    #[cfg(feature = "interactive_mp")]
    fn qwerty() {
        use crate::{
            aggregate_public_key_shares, aggregate_server_key_shares,
            bool::keys::ServerKeyEvaluationDomain,
            evaluator::MultiPartyCrs,
            gen_client_key, gen_mp_keys_phase1, gen_mp_keys_phase2,
            parameters::{BoolParameters, CiphertextModulus},
            random::DefaultSecureRng,
            set_mp_seed, set_parameter_set,
            utils::WithLocal,
            BoolEvaluator, DefaultDecomposer, ModularOpsU64, Ntt, NttBackendU64,
        };

        set_parameter_set(crate::ParameterSelector::HighCommunicationButFast2Party);
        set_mp_seed(MultiPartyCrs::random().seed);
        let parties = 2;
        let cks = (0..parties).map(|_| gen_client_key()).collect_vec();
        let pk_shares = cks.iter().map(|k| gen_mp_keys_phase1(k)).collect_vec();
        let pk = aggregate_public_key_shares(&pk_shares);
        let server_key_shares = cks
            .iter()
            .enumerate()
            .map(|(index, k)| gen_mp_keys_phase2(k, index, parties, &pk))
            .collect_vec();
        let seeded_server_key = aggregate_server_key_shares(&server_key_shares);
        let server_key_eval =
            ServerKeyEvaluationDomain::<_, _, DefaultSecureRng, NttBackendU64>::from(
                &seeded_server_key,
            );

        let parameters = BoolEvaluator::with_local(|e| e.parameters().clone());
        let server_key_stats = collect_server_key_stats::<
            _,
            DefaultDecomposer<u64>,
            NttBackendU64,
            ModularOpsU64<CiphertextModulus<u64>>,
            _,
        >(parameters, &cks, &server_key_eval);

        println!(
            "Rgsw nsm std log2 {}",
            server_key_stats.brk_rgsw_cts.0.std_dev().abs().log2()
        );
        println!(
            "Rgsw m std log2 {}",
            server_key_stats.brk_rgsw_cts.1.std_dev().abs().log2()
        );
        println!(
            "rlwe post 1 auto std log2 {}",
            server_key_stats.post_1_auto.std_dev().abs().log2()
        );
        println!(
            "key switching noise rlwe secret s to lwe secret z std log2 {}",
            server_key_stats.post_lwe_key_switch.std_dev().abs().log2()
        );
    }

    #[test]
    #[cfg(feature = "non_interactive_mp")]
    fn querty2() {
        use crate::{
            aggregate_server_key_shares, bool::keys::NonInteractiveServerKeyEvaluationDomain,
            evaluator::NonInteractiveMultiPartyCrs, gen_client_key, gen_server_key_share,
            parameters::CiphertextModulus, random::DefaultSecureRng, set_common_reference_seed,
            set_parameter_set, utils::WithLocal, BoolEvaluator, DefaultDecomposer, ModularOpsU64,
            NttBackendU64,
        };

        set_parameter_set(crate::ParameterSelector::NonInteractiveLTE2Party);
        set_common_reference_seed(NonInteractiveMultiPartyCrs::random().seed);
        let parties = 2;
        let cks = (0..parties).map(|_| gen_client_key()).collect_vec();
        let server_key_shares = cks
            .iter()
            .enumerate()
            .map(|(user_id, k)| gen_server_key_share(user_id, parties, k))
            .collect_vec();
        let server_key = aggregate_server_key_shares(&server_key_shares);

        let server_key_eval =
            NonInteractiveServerKeyEvaluationDomain::<_, _, DefaultSecureRng, NttBackendU64>::from(
                &server_key,
            );

        let parameters = BoolEvaluator::with_local(|e| e.parameters().clone());
        let server_key_stats = collect_server_key_stats::<
            _,
            DefaultDecomposer<u64>,
            NttBackendU64,
            ModularOpsU64<CiphertextModulus<u64>>,
            _,
        >(parameters, &cks, &server_key_eval);

        println!(
            "Rgsw nsm std log2 {}",
            server_key_stats.brk_rgsw_cts.0.std_dev().abs().log2()
        );
        println!(
            "Rgsw m std log2 {}",
            server_key_stats.brk_rgsw_cts.1.std_dev().abs().log2()
        );
        println!(
            "rlwe post 1 auto std log2 {}",
            server_key_stats.post_1_auto.std_dev().abs().log2()
        );
        println!(
            "key switching noise rlwe secret s to lwe secret z std log2 {}",
            server_key_stats.post_lwe_key_switch.std_dev().abs().log2()
        );
    }

    #[test]
    #[cfg(feature = "non_interactive_mp")]
    fn enc_under_sk_and_key_switch() {
        use rand::{thread_rng, Rng};

        use crate::{
            aggregate_server_key_shares,
            bool::{keys::tests::ideal_sk_rlwe, ni_mp_api::NonInteractiveBatchedFheBools},
            gen_client_key, gen_server_key_share,
            rgsw::decrypt_rlwe,
            set_common_reference_seed, set_parameter_set,
            utils::{tests::Stats, TryConvertFrom1, WithLocal},
            BoolEvaluator, Encoder, Encryptor, KeySwitchWithId, ModInit, ModularOpsU64,
            NttBackendU64, NttInit, ParameterSelector, VectorOps,
        };

        set_parameter_set(ParameterSelector::NonInteractiveLTE2Party);
        set_common_reference_seed([2; 32]);

        let parties = 2;

        let cks = (0..parties).map(|_| gen_client_key()).collect_vec();

        let key_shares = cks
            .iter()
            .enumerate()
            .map(|(user_index, ck)| gen_server_key_share(user_index, parties, ck))
            .collect_vec();

        let seeded_server_key = aggregate_server_key_shares(&key_shares);
        seeded_server_key.set_server_key();

        let parameters = BoolEvaluator::with_local(|e| e.parameters().clone());
        let nttop = NttBackendU64::new(parameters.rlwe_q(), parameters.rlwe_n().0);
        let rlwe_q_modop = ModularOpsU64::new(*parameters.rlwe_q());

        let m = (0..parameters.rlwe_n().0)
            .map(|_| thread_rng().gen_bool(0.5))
            .collect_vec();
        let ct: NonInteractiveBatchedFheBools<_> = cks[0].encrypt(m.as_slice());
        let ct = ct.key_switch(0);

        let ideal_rlwe_sk = ideal_sk_rlwe(&cks);

        let message = m
            .iter()
            .map(|b| parameters.rlwe_q().encode(*b))
            .collect_vec();

        let mut m_out = vec![0u64; parameters.rlwe_n().0];
        decrypt_rlwe(
            &ct.data[0],
            &ideal_rlwe_sk,
            &mut m_out,
            &nttop,
            &rlwe_q_modop,
        );

        let mut diff = m_out;
        rlwe_q_modop.elwise_sub_mut(diff.as_mut_slice(), message.as_ref());

        let mut stats = Stats::new();
        stats.add_more(&Vec::<i64>::try_convert_from(
            diff.as_slice(),
            parameters.rlwe_q(),
        ));
        println!("Noise std log2: {}", stats.std_dev().abs().log2());
    }
}
