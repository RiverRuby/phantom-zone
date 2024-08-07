use std::fmt::Debug;

use itertools::izip;
use num_traits::Zero;

use crate::{
    backend::{GetModulus, Modulus, VectorOps},
    ntt::Ntt,
    random::{
        RandomFillGaussianInModulus, RandomFillUniformInModulus, RandomGaussianElementInModulus,
    },
    utils::TryConvertFrom1,
    ArithmeticOps, Matrix, MatrixEntity, MatrixMut, Row, RowEntity, RowMut,
};

pub(crate) fn public_key_share<
    R: Row + RowMut + RowEntity,
    S,
    ModOp: VectorOps<Element = R::Element> + GetModulus<Element = R::Element>,
    NttOp: Ntt<Element = R::Element>,
    Rng: RandomFillGaussianInModulus<[R::Element], ModOp::M>,
    PRng: RandomFillUniformInModulus<[R::Element], ModOp::M>,
>(
    share_out: &mut R,
    s_i: &[S],
    modop: &ModOp,
    nttop: &NttOp,
    p_rng: &mut PRng,
    rng: &mut Rng,
) where
    R: TryConvertFrom1<[S], ModOp::M>,
{
    let ring_size = share_out.as_ref().len();
    assert!(s_i.len() == ring_size);

    let q = modop.modulus();

    // sample a
    let mut a = {
        let mut a = R::zeros(ring_size);
        RandomFillUniformInModulus::random_fill(p_rng, &q, a.as_mut());
        a
    };

    // s*a
    nttop.forward(a.as_mut());
    let mut s = R::try_convert_from(s_i, &q);
    nttop.forward(s.as_mut());
    modop.elwise_mul_mut(s.as_mut(), a.as_ref());
    nttop.backward(s.as_mut());

    RandomFillGaussianInModulus::random_fill(rng, &q, share_out.as_mut());
    modop.elwise_add_mut(share_out.as_mut(), s.as_ref()); // s*e + e
}

/// Generate decryption share for LWE ciphertext `lwe_ct` with user's secret `s`
pub(crate) fn multi_party_decryption_share<
    R: RowMut + RowEntity,
    Mod: Modulus<Element = R::Element>,
    ModOp: ArithmeticOps<Element = R::Element> + VectorOps<Element = R::Element> + GetModulus<M = Mod>,
    Rng: RandomGaussianElementInModulus<R::Element, Mod>,
    S,
>(
    lwe_ct: &R,
    s: &[S],
    mod_op: &ModOp,
    rng: &mut Rng,
) -> R::Element
where
    R: TryConvertFrom1<[S], Mod>,
    R::Element: Zero,
{
    assert!(lwe_ct.as_ref().len() == s.len() + 1);
    let mut neg_s = R::try_convert_from(s, mod_op.modulus());
    mod_op.elwise_neg_mut(neg_s.as_mut());

    // share =  (\sum -s_i * a_i) + e
    let mut share = R::Element::zero();
    izip!(neg_s.as_ref().iter(), lwe_ct.as_ref().iter().skip(1)).for_each(|(si, ai)| {
        share = mod_op.add(&share, &mod_op.mul(si, ai));
    });

    let e = rng.random(mod_op.modulus());
    share = mod_op.add(&share, &e);

    share
}

/// Aggregate decryption shares for `lwe_ct` and return noisy decryption output
/// `m + e`
pub(crate) fn multi_party_aggregate_decryption_shares_and_decrypt<
    R: RowMut + RowEntity,
    ModOp: ArithmeticOps<Element = R::Element>,
>(
    lwe_ct: &R,
    shares: &[R::Element],
    mod_op: &ModOp,
) -> R::Element
where
    R::Element: Zero,
{
    let mut sum_shares = R::Element::zero();
    shares
        .iter()
        .for_each(|v| sum_shares = mod_op.add(&sum_shares, v));
    mod_op.add(&lwe_ct.as_ref()[0], &sum_shares)
}

pub(crate) fn non_interactive_rgsw_ct<
    M: MatrixMut + MatrixEntity,
    S,
    PRng: RandomFillUniformInModulus<[M::MatElement], ModOp::M>,
    Rng: RandomFillGaussianInModulus<[M::MatElement], ModOp::M>,
    NttOp: Ntt<Element = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement> + GetModulus<Element = M::MatElement>,
>(
    s: &[S],
    u: &[S],
    m: &[M::MatElement],
    gadget_vec: &[M::MatElement],
    p_rng: &mut PRng,
    rng: &mut Rng,
    nttop: &NttOp,
    modop: &ModOp,
) -> (M, M)
where
    <M as Matrix>::R: RowMut + TryConvertFrom1<[S], ModOp::M> + RowEntity,
    M::MatElement: Copy,
{
    assert_eq!(s.len(), u.len());
    assert_eq!(s.len(), m.len());
    let q = modop.modulus();
    let d = gadget_vec.len();
    let ring_size = s.len();

    let mut s_poly_eval = M::R::try_convert_from(s, q);
    let mut u_poly_eval = M::R::try_convert_from(u, q);
    nttop.forward(s_poly_eval.as_mut());
    nttop.forward(u_poly_eval.as_mut());

    // encryptions of a_i*u + e + \beta m
    let mut enc_beta_m = M::zeros(d, ring_size);
    // zero encrypition: a_i*s + e'
    let mut zero_encryptions = M::zeros(d, ring_size);

    let mut scratch_space = M::R::zeros(ring_size);

    izip!(
        enc_beta_m.iter_rows_mut(),
        zero_encryptions.iter_rows_mut(),
        gadget_vec.iter()
    )
    .for_each(|(e_beta_m, e_zero, beta)| {
        // sample a_i
        RandomFillUniformInModulus::random_fill(p_rng, q, e_beta_m.as_mut());
        e_zero.as_mut().copy_from_slice(e_beta_m.as_ref());

        // a_i * u + \beta m + e //
        // a_i * u
        nttop.forward(e_beta_m.as_mut());
        modop.elwise_mul_mut(e_beta_m.as_mut(), u_poly_eval.as_ref());
        nttop.backward(e_beta_m.as_mut());
        // sample error e
        RandomFillGaussianInModulus::random_fill(rng, q, scratch_space.as_mut());
        // a_i * u + e
        modop.elwise_add_mut(e_beta_m.as_mut(), scratch_space.as_ref());
        // beta * m
        modop.elwise_scalar_mul(scratch_space.as_mut(), m.as_ref(), beta);
        // a_i * u + e + \beta m
        modop.elwise_add_mut(e_beta_m.as_mut(), scratch_space.as_ref());

        // a_i * s + e //
        // a_i * s
        nttop.forward(e_zero.as_mut());
        modop.elwise_mul_mut(e_zero.as_mut(), s_poly_eval.as_ref());
        nttop.backward(e_zero.as_mut());
        // sample error e
        RandomFillGaussianInModulus::random_fill(rng, q, scratch_space.as_mut());
        // a_i * s + e
        modop.elwise_add_mut(e_zero.as_mut(), scratch_space.as_ref());
    });

    (enc_beta_m, zero_encryptions)
}

pub(crate) fn non_interactive_ksk_gen<
    M: MatrixMut + MatrixEntity,
    S,
    PRng: RandomFillUniformInModulus<[M::MatElement], ModOp::M>,
    Rng: RandomFillGaussianInModulus<[M::MatElement], ModOp::M>,
    NttOp: Ntt<Element = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement> + GetModulus<Element = M::MatElement>,
>(
    s: &[S],
    u: &[S],
    gadget_vec: &[M::MatElement],
    p_rng: &mut PRng,
    rng: &mut Rng,
    nttop: &NttOp,
    modop: &ModOp,
) -> M
where
    <M as Matrix>::R: RowMut + TryConvertFrom1<[S], ModOp::M> + RowEntity,
    M::MatElement: Copy + Debug,
{
    assert_eq!(s.len(), u.len());

    let q = modop.modulus();
    let d = gadget_vec.len();
    let ring_size = s.len();

    let mut s_poly_eval = M::R::try_convert_from(s, q);
    nttop.forward(s_poly_eval.as_mut());
    let u_poly = M::R::try_convert_from(u, q);
    // a_i * s + \beta u + e
    let mut ksk = M::zeros(d, ring_size);

    let mut scratch_space = M::R::zeros(ring_size);

    izip!(ksk.iter_rows_mut(), gadget_vec.iter()).for_each(|(e_ksk, beta)| {
        // sample a_i
        RandomFillUniformInModulus::random_fill(p_rng, q, e_ksk.as_mut());

        // a_i * s + e + beta u
        nttop.forward(e_ksk.as_mut());
        modop.elwise_mul_mut(e_ksk.as_mut(), s_poly_eval.as_ref());
        nttop.backward(e_ksk.as_mut());
        // sample error e
        RandomFillGaussianInModulus::random_fill(rng, q, scratch_space.as_mut());
        // a_i * s + e
        modop.elwise_add_mut(e_ksk.as_mut(), scratch_space.as_ref());
        // \beta * u
        modop.elwise_scalar_mul(scratch_space.as_mut(), u_poly.as_ref(), beta);
        // a_i * s + e + \beta * u
        modop.elwise_add_mut(e_ksk.as_mut(), scratch_space.as_ref());
    });

    ksk
}

pub(crate) fn non_interactive_ksk_zero_encryptions_for_other_party_i<
    M: MatrixMut + MatrixEntity,
    S,
    PRng: RandomFillUniformInModulus<[M::MatElement], ModOp::M>,
    Rng: RandomFillGaussianInModulus<[M::MatElement], ModOp::M>,
    NttOp: Ntt<Element = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement> + GetModulus<Element = M::MatElement>,
>(
    s: &[S],
    gadget_vec: &[M::MatElement],
    p_rng: &mut PRng,
    rng: &mut Rng,
    nttop: &NttOp,
    modop: &ModOp,
) -> M
where
    <M as Matrix>::R: RowMut + TryConvertFrom1<[S], ModOp::M> + RowEntity,
    M::MatElement: Copy + Debug,
{
    let q = modop.modulus();
    let d = gadget_vec.len();
    let ring_size = s.len();

    let mut s_poly_eval = M::R::try_convert_from(s, q);
    nttop.forward(s_poly_eval.as_mut());

    // a_i * s + e
    let mut zero_encs = M::zeros(d, ring_size);

    let mut scratch_space = M::R::zeros(ring_size);

    izip!(zero_encs.iter_rows_mut()).for_each(|e_zero| {
        // sample a_i
        RandomFillUniformInModulus::random_fill(p_rng, q, e_zero.as_mut());

        // a_i * s + e
        nttop.forward(e_zero.as_mut());
        modop.elwise_mul_mut(e_zero.as_mut(), s_poly_eval.as_ref());
        nttop.backward(e_zero.as_mut());
        // sample error e
        RandomFillGaussianInModulus::random_fill(rng, q, scratch_space.as_mut());
        modop.elwise_add_mut(e_zero.as_mut(), scratch_space.as_ref());
    });

    zero_encs
}
