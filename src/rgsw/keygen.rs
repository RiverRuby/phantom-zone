use std::{fmt::Debug, ops::Sub};

use itertools::izip;
use num_traits::{PrimInt, Signed, ToPrimitive, Zero};

use crate::{
    backend::{ArithmeticOps, GetModulus, Modulus, VectorOps},
    ntt::Ntt,
    random::{
        RandomElementInModulus, RandomFill, RandomFillGaussianInModulus, RandomFillUniformInModulus,
    },
    utils::{fill_random_ternary_secret_with_hamming_weight, TryConvertFrom1},
    Matrix, MatrixEntity, MatrixMut, Row, RowEntity, RowMut,
};

pub(crate) fn generate_auto_map(ring_size: usize, k: isize) -> (Vec<usize>, Vec<bool>) {
    assert!(k & 1 == 1, "Auto {k} must be odd");

    let k = if k < 0 {
        // k is -ve, return k%(2*N)
        (2 * ring_size) - (k.abs() as usize % (2 * ring_size))
    } else {
        k as usize
    };
    let (auto_map_index, auto_sign_index): (Vec<usize>, Vec<bool>) = (0..ring_size)
        .into_iter()
        .map(|i| {
            let mut to_index = (i * k) % (2 * ring_size);
            let mut sign = true;

            // wrap around. false implies negative
            if to_index >= ring_size {
                to_index = to_index - ring_size;
                sign = false;
            }

            (to_index, sign)
        })
        .unzip();
    (auto_map_index, auto_sign_index)
}

/// Returns RGSW(m)
///
/// RGSW = [RLWE'(-sm) || RLWE(m)] = [RLWE'_A(-sm), RLWE'_B(-sm), RLWE'_A(m),
/// RLWE'_B(m)]
///
/// RGSW(m1) ciphertext is used for RLWE(m0) x RGSW(m1) multiplication.
/// Let RLWE(m) = [a, b] where b = as + e + m0.
/// For RLWExRGSW we calculate:
///       (\sum signed_decompose(a)[i] x RLWE(-s \beta^i' m1))
///     + (\sum signed_decompose(b)[i'] x RLWE(\beta^i' m1))
///     = RLWE(m0m1)
/// We denote decomposer count for signed_decompose(a)[i] with d_a and
/// corresponding gadget vector with `gadget_a`. We denote decomposer count for
/// signed_decompose(b)[i] with d_b and corresponding gadget vector with
/// `gadget_b`
///
/// In secret key RGSW encrypton RLWE'_A(m) can be seeded. Hence, we seed it
/// using the `p_rng` passed and the retured RGSW ciphertext has d_a * 2 + d_b
/// rows
///
/// - s: is the secret key
/// - m: message to encrypt
/// - gadget_a: Gadget vector for RLWE'(-sm)
/// - gadget_b: Gadget vector for RLWE'(m)
/// - p_rng: Seeded psuedo random generator used to sample RLWE'_A(m).
pub(crate) fn secret_key_encrypt_rgsw<
    Mmut: MatrixMut + MatrixEntity,
    S,
    R: RandomFillGaussianInModulus<[Mmut::MatElement], ModOp::M>
        + RandomFillUniformInModulus<[Mmut::MatElement], ModOp::M>,
    PR: RandomFillUniformInModulus<[Mmut::MatElement], ModOp::M>,
    ModOp: VectorOps<Element = Mmut::MatElement> + GetModulus<Element = Mmut::MatElement>,
    NttOp: Ntt<Element = Mmut::MatElement>,
>(
    out_rgsw: &mut Mmut,
    m: &[Mmut::MatElement],
    gadget_a: &[Mmut::MatElement],
    gadget_b: &[Mmut::MatElement],
    s: &[S],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    p_rng: &mut PR,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut + RowEntity + TryConvertFrom1<[S], ModOp::M> + Debug,
    Mmut::MatElement: Copy + Debug,
{
    let d_a = gadget_a.len();
    let d_b = gadget_b.len();
    let q = mod_op.modulus();
    let ring_size = s.len();
    assert!(out_rgsw.dimension() == (d_a * 2 + d_b, ring_size));
    assert!(m.as_ref().len() == ring_size);

    // RLWE(-sm), RLWE(m)
    let (rlwe_dash_nsm, b_rlwe_dash_m) = out_rgsw.split_at_row_mut(d_a * 2);

    let mut s_eval = Mmut::R::try_convert_from(s, &q);
    ntt_op.forward(s_eval.as_mut());

    let mut scratch_space = Mmut::R::zeros(ring_size);

    // RLWE'(-sm)
    let (a_rlwe_dash_nsm, b_rlwe_dash_nsm) = rlwe_dash_nsm.split_at_mut(d_a);
    izip!(
        a_rlwe_dash_nsm.iter_mut(),
        b_rlwe_dash_nsm.iter_mut(),
        gadget_a.iter()
    )
    .for_each(|(ai, bi, beta_i)| {
        // Sample a_i
        RandomFillUniformInModulus::random_fill(rng, &q, ai.as_mut());

        // a_i * s
        scratch_space.as_mut().copy_from_slice(ai.as_ref());
        ntt_op.forward(scratch_space.as_mut());
        mod_op.elwise_mul_mut(scratch_space.as_mut(), s_eval.as_ref());
        ntt_op.backward(scratch_space.as_mut());

        // b_i = e_i + a_i * s
        RandomFillGaussianInModulus::random_fill(rng, &q, bi.as_mut());
        mod_op.elwise_add_mut(bi.as_mut(), scratch_space.as_ref());

        // a_i + \beta_i * m
        mod_op.elwise_scalar_mul(scratch_space.as_mut(), m.as_ref(), beta_i);
        mod_op.elwise_add_mut(ai.as_mut(), scratch_space.as_ref());
    });

    // RLWE(m)
    let mut a_rlwe_dash_m = {
        // polynomials of part A of RLWE'(m) are sampled from seed
        let mut a = Mmut::zeros(d_b, ring_size);
        a.iter_rows_mut()
            .for_each(|ai| RandomFillUniformInModulus::random_fill(p_rng, &q, ai.as_mut()));
        a
    };

    izip!(
        a_rlwe_dash_m.iter_rows_mut(),
        b_rlwe_dash_m.iter_mut(),
        gadget_b.iter()
    )
    .for_each(|(ai, bi, beta_i)| {
        // ai * s
        ntt_op.forward(ai.as_mut());
        mod_op.elwise_mul_mut(ai.as_mut(), s_eval.as_ref());
        ntt_op.backward(ai.as_mut());

        // beta_i * m
        mod_op.elwise_scalar_mul(scratch_space.as_mut(), m.as_ref(), beta_i);

        // Sample e_i
        RandomFillGaussianInModulus::random_fill(rng, &q, bi.as_mut());
        // e_i + beta_i * m + ai*s
        mod_op.elwise_add_mut(bi.as_mut(), scratch_space.as_ref());
        mod_op.elwise_add_mut(bi.as_mut(), ai.as_ref());
    });
}

/// Returns RGSW(m) encrypted with public key
///
/// Follows the same routine as `secret_key_encrypt_rgsw` but with the
/// difference that each RLWE encryption uses public key instead of secret key.
///
/// Since public key encryption cannot be seeded `RLWE'_A(m)` is included in the
/// ciphertext. Hence the returned RGSW ciphertext has d_a * 2 + d_b * 2 rows
pub(crate) fn public_key_encrypt_rgsw<
    Mmut: MatrixMut + MatrixEntity,
    M: Matrix<MatElement = Mmut::MatElement>,
    R: RandomFillGaussianInModulus<[Mmut::MatElement], ModOp::M>
        + RandomFill<[u8]>
        + RandomElementInModulus<usize, usize>,
    ModOp: VectorOps<Element = Mmut::MatElement> + GetModulus<Element = Mmut::MatElement>,
    NttOp: Ntt<Element = Mmut::MatElement>,
>(
    out_rgsw: &mut Mmut,
    m: &[M::MatElement],
    public_key: &M,
    gadget_a: &[Mmut::MatElement],
    gadget_b: &[Mmut::MatElement],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut + RowEntity + TryConvertFrom1<[i32], ModOp::M>,
    Mmut::MatElement: Copy,
{
    let ring_size = public_key.dimension().1;
    let d_a = gadget_a.len();
    let d_b = gadget_b.len();
    assert!(public_key.dimension().0 == 2);
    assert!(out_rgsw.dimension() == (d_a * 2 + d_b * 2, ring_size));

    let mut pk_eval = Mmut::zeros(2, ring_size);
    izip!(pk_eval.iter_rows_mut(), public_key.iter_rows()).for_each(|(to_i, from_i)| {
        to_i.as_mut().copy_from_slice(from_i.as_ref());
        ntt_op.forward(to_i.as_mut());
    });
    let p0 = pk_eval.get_row_slice(0);
    let p1 = pk_eval.get_row_slice(1);

    let q = mod_op.modulus();

    // RGSW(m) = RLWE'(-sm), RLWE(m)
    let (rlwe_dash_nsm, rlwe_dash_m) = out_rgsw.split_at_row_mut(d_a * 2);

    // RLWE(-sm)
    let (rlwe_dash_nsm_parta, rlwe_dash_nsm_partb) = rlwe_dash_nsm.split_at_mut(d_a);
    izip!(
        rlwe_dash_nsm_parta.iter_mut(),
        rlwe_dash_nsm_partb.iter_mut(),
        gadget_a.iter()
    )
    .for_each(|(ai, bi, beta_i)| {
        // sample ephemeral secret u_i
        let mut u = vec![0i32; ring_size];
        fill_random_ternary_secret_with_hamming_weight(u.as_mut(), ring_size >> 1, rng);
        let mut u_eval = Mmut::R::try_convert_from(u.as_ref(), &q);
        ntt_op.forward(u_eval.as_mut());

        let mut u_eval_copy = Mmut::R::zeros(ring_size);
        u_eval_copy.as_mut().copy_from_slice(u_eval.as_ref());

        // p0 * u
        mod_op.elwise_mul_mut(u_eval.as_mut(), p0.as_ref());
        // p1 * u
        mod_op.elwise_mul_mut(u_eval_copy.as_mut(), p1.as_ref());
        ntt_op.backward(u_eval.as_mut());
        ntt_op.backward(u_eval_copy.as_mut());

        // sample error
        RandomFillGaussianInModulus::random_fill(rng, &q, ai.as_mut());
        RandomFillGaussianInModulus::random_fill(rng, &q, bi.as_mut());

        // a = p0*u+e0
        mod_op.elwise_add_mut(ai.as_mut(), u_eval.as_ref());
        // b = p1*u+e1
        mod_op.elwise_add_mut(bi.as_mut(), u_eval_copy.as_ref());

        // a = p0*u + e0 + \beta*m
        // use u_eval as scratch
        mod_op.elwise_scalar_mul(u_eval.as_mut(), m.as_ref(), beta_i);
        mod_op.elwise_add_mut(ai.as_mut(), u_eval.as_ref());
    });

    // RLWE(m)
    let (rlwe_dash_m_parta, rlwe_dash_m_partb) = rlwe_dash_m.split_at_mut(d_b);
    izip!(
        rlwe_dash_m_parta.iter_mut(),
        rlwe_dash_m_partb.iter_mut(),
        gadget_b.iter()
    )
    .for_each(|(ai, bi, beta_i)| {
        // sample ephemeral secret u_i
        let mut u = vec![0i32; ring_size];
        fill_random_ternary_secret_with_hamming_weight(u.as_mut(), ring_size >> 1, rng);
        let mut u_eval = Mmut::R::try_convert_from(u.as_ref(), &q);
        ntt_op.forward(u_eval.as_mut());

        let mut u_eval_copy = Mmut::R::zeros(ring_size);
        u_eval_copy.as_mut().copy_from_slice(u_eval.as_ref());

        // p0 * u
        mod_op.elwise_mul_mut(u_eval.as_mut(), p0.as_ref());
        // p1 * u
        mod_op.elwise_mul_mut(u_eval_copy.as_mut(), p1.as_ref());
        ntt_op.backward(u_eval.as_mut());
        ntt_op.backward(u_eval_copy.as_mut());

        // sample error
        RandomFillGaussianInModulus::random_fill(rng, &q, ai.as_mut());
        RandomFillGaussianInModulus::random_fill(rng, &q, bi.as_mut());

        // a = p0*u+e0
        mod_op.elwise_add_mut(ai.as_mut(), u_eval.as_ref());
        // b = p1*u+e1
        mod_op.elwise_add_mut(bi.as_mut(), u_eval_copy.as_ref());

        // b = p1*u + e0 + \beta*m
        // use u_eval as scratch
        mod_op.elwise_scalar_mul(u_eval.as_mut(), m.as_ref(), beta_i);
        mod_op.elwise_add_mut(bi.as_mut(), u_eval.as_ref());
    });
}

/// Returns key switching key to key switch ciphertext RLWE_{from_s}(m)
/// to RLWE_{to_s}(m).
///
/// Let key switching decomposer have `d` decompostion count with gadget vector:
/// [1, \beta, ..., \beta^d-1]
///
/// Key switching key consists of `d` RLWE ciphertexts:
/// RLWE'_{to_s}(-from_s) = [RLWE_{to_s}(\beta^i -from_s)]
///
/// In RLWE(m) s.t. b = as + e + m where s is the secret key, `a` can be seeded.
/// And we seed all RLWE ciphertexts in key switchin key.
///
/// - neg_from_s: Negative of secret polynomial to key switch from (i.e.
///   -from_s)
/// - to_s: secret polynomial to key switch to.
/// - gadget_vector: Gadget vector of decomposer used in key switch
/// - p_rng: Seeded pseudo random generate used to generate `a` polynomials of
///   key switching key RLWE ciphertexts
fn seeded_rlwe_ksk_gen<
    Mmut: MatrixMut + MatrixEntity,
    ModOp: ArithmeticOps<Element = Mmut::MatElement>
        + VectorOps<Element = Mmut::MatElement>
        + GetModulus<Element = Mmut::MatElement>,
    NttOp: Ntt<Element = Mmut::MatElement>,
    R: RandomFillGaussianInModulus<[Mmut::MatElement], ModOp::M>,
    PR: RandomFillUniformInModulus<[Mmut::MatElement], ModOp::M>,
>(
    ksk_out: &mut Mmut,
    neg_from_s: Mmut::R,
    mut to_s: Mmut::R,
    gadget_vector: &[Mmut::MatElement],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    p_rng: &mut PR,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut,
{
    let ring_size = neg_from_s.as_ref().len();
    let d = gadget_vector.len();
    assert!(ksk_out.dimension() == (d, ring_size));

    let q = mod_op.modulus();

    ntt_op.forward(to_s.as_mut());

    // RLWE'_{to_s}(-from_s)
    let mut part_a = {
        let mut a = Mmut::zeros(d, ring_size);
        a.iter_rows_mut()
            .for_each(|ai| RandomFillUniformInModulus::random_fill(p_rng, q, ai.as_mut()));
        a
    };
    izip!(
        part_a.iter_rows_mut(),
        ksk_out.iter_rows_mut(),
        gadget_vector.iter(),
    )
    .for_each(|(ai, bi, beta_i)| {
        // si * ai
        ntt_op.forward(ai.as_mut());
        mod_op.elwise_mul_mut(ai.as_mut(), to_s.as_ref());
        ntt_op.backward(ai.as_mut());

        // ei + to_s*ai
        RandomFillGaussianInModulus::random_fill(rng, &q, bi.as_mut());
        mod_op.elwise_add_mut(bi.as_mut(), ai.as_ref());

        // beta_i * -from_s
        // use ai as scratch space
        mod_op.elwise_scalar_mul(ai.as_mut(), neg_from_s.as_ref(), beta_i);

        // bi = ei + to_s*ai + beta_i*-from_s
        mod_op.elwise_add_mut(bi.as_mut(), ai.as_ref());
    });
}

/// Returns auto key to send RLWE(m(X)) -> RLWE(m(X^k))
///
/// Auto key is key switchin key that key-switches RLWE_{s(X^k)}(m(X^k)) to
/// RLWE_{s(X)}(m(X^k)).
///
/// - s: secret polynomial s(X)
/// - auto_k: k used in for autmorphism X -> X^k
/// - gadget_vector: Gadget vector corresponding to decomposer used in key
///   switch
/// - p_rng: pseudo random generator used to generate `a` polynomials of key
///   switching key RLWE ciphertexts
pub(crate) fn seeded_auto_key_gen<
    Mmut: MatrixMut + MatrixEntity,
    ModOp: ArithmeticOps<Element = Mmut::MatElement>
        + VectorOps<Element = Mmut::MatElement>
        + GetModulus<Element = Mmut::MatElement>,
    NttOp: Ntt<Element = Mmut::MatElement>,
    S,
    R: RandomFillGaussianInModulus<[Mmut::MatElement], ModOp::M>,
    PR: RandomFillUniformInModulus<[Mmut::MatElement], ModOp::M>,
>(
    ksk_out: &mut Mmut,
    s: &[S],
    auto_k: isize,
    gadget_vector: &[Mmut::MatElement],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    p_rng: &mut PR,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut,
    Mmut::R: TryConvertFrom1<[S], ModOp::M> + RowEntity,
    Mmut::MatElement: Copy + Sub<Output = Mmut::MatElement>,
{
    let ring_size = s.len();
    let (auto_map_index, auto_map_sign) = generate_auto_map(ring_size, auto_k);

    let q = mod_op.modulus();

    // s(X) -> -s(X^k)
    let s = Mmut::R::try_convert_from(s, q);
    let mut neg_s_auto = Mmut::R::zeros(s.as_ref().len());
    izip!(s.as_ref(), auto_map_index.iter(), auto_map_sign.iter()).for_each(
        |(el, to_index, sign)| {
            // if sign is +ve (true), then negate because we need -s(X) (i.e. do the
            // opposite than the usual case)
            if *sign {
                neg_s_auto.as_mut()[*to_index] = mod_op.neg(el);
            } else {
                neg_s_auto.as_mut()[*to_index] = *el;
            }
        },
    );

    // Ksk from -s(X^k) to s(X)
    seeded_rlwe_ksk_gen(
        ksk_out,
        neg_s_auto,
        s,
        gadget_vector,
        mod_op,
        ntt_op,
        p_rng,
        rng,
    );
}

/// Returns seeded RLWE(m(X))
///
/// RLWE(m(X)) = [a(X), b(X) = a(X)s(X) + e(X) + m(X)]
///
/// a(X) of RLWE encyrptions using secret key s(X) can be seeded. We use seeded
/// pseudo random generator `p_rng` to sample a(X) and return seeded RLWE
/// ciphertext (i.e. only b(X))
pub(crate) fn seeded_secret_key_encrypt_rlwe<
    Ro: Row + RowMut + RowEntity,
    ModOp: VectorOps<Element = Ro::Element> + GetModulus<Element = Ro::Element>,
    NttOp: Ntt<Element = Ro::Element>,
    S,
    R: RandomFillGaussianInModulus<[Ro::Element], ModOp::M>,
    PR: RandomFillUniformInModulus<[Ro::Element], ModOp::M>,
>(
    m: &[Ro::Element],
    b_rlwe_out: &mut Ro,
    s: &[S],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    p_rng: &mut PR,
    rng: &mut R,
) where
    Ro: TryConvertFrom1<[S], ModOp::M> + Debug,
{
    let ring_size = s.len();
    assert!(m.as_ref().len() == ring_size);
    assert!(b_rlwe_out.as_ref().len() == ring_size);

    let q = mod_op.modulus();

    // sample a
    let mut a = {
        let mut a = Ro::zeros(ring_size);
        RandomFillUniformInModulus::random_fill(p_rng, q, a.as_mut());
        a
    };

    // s * a
    let mut sa = Ro::try_convert_from(s, q);
    ntt_op.forward(sa.as_mut());
    ntt_op.forward(a.as_mut());
    mod_op.elwise_mul_mut(sa.as_mut(), a.as_ref());
    ntt_op.backward(sa.as_mut());

    // sample e
    RandomFillGaussianInModulus::random_fill(rng, q, b_rlwe_out.as_mut());
    mod_op.elwise_add_mut(b_rlwe_out.as_mut(), m.as_ref());
    mod_op.elwise_add_mut(b_rlwe_out.as_mut(), sa.as_ref());
}

/// Returns RLWE(m(X)) encrypted using public key.
///
/// Unlike secret key encryption, public key encryption cannot be seeded
pub(crate) fn public_key_encrypt_rlwe<
    M: Matrix,
    Mmut: MatrixMut<MatElement = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement> + GetModulus<Element = M::MatElement>,
    NttOp: Ntt<Element = M::MatElement>,
    S,
    R: RandomFillGaussianInModulus<[M::MatElement], ModOp::M>
        + RandomFillUniformInModulus<[M::MatElement], ModOp::M>
        + RandomFill<[u8]>
        + RandomElementInModulus<usize, usize>,
>(
    rlwe_out: &mut Mmut,
    pk: &M,
    m: &[M::MatElement],
    mod_op: &ModOp,
    ntt_op: &NttOp,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut + TryConvertFrom1<[S], ModOp::M> + RowEntity,
    M::MatElement: Copy,
    S: Zero + Signed + Copy,
{
    let ring_size = m.len();
    assert!(rlwe_out.dimension() == (2, ring_size));

    let q = mod_op.modulus();

    let mut u = vec![S::zero(); ring_size];
    fill_random_ternary_secret_with_hamming_weight(u.as_mut(), ring_size >> 1, rng);
    let mut u = Mmut::R::try_convert_from(&u, q);
    ntt_op.forward(u.as_mut());

    let mut ua = Mmut::R::zeros(ring_size);
    ua.as_mut().copy_from_slice(pk.get_row_slice(0));
    let mut ub = Mmut::R::zeros(ring_size);
    ub.as_mut().copy_from_slice(pk.get_row_slice(1));

    // a*u
    ntt_op.forward(ua.as_mut());
    mod_op.elwise_mul_mut(ua.as_mut(), u.as_ref());
    ntt_op.backward(ua.as_mut());

    // b*u
    ntt_op.forward(ub.as_mut());
    mod_op.elwise_mul_mut(ub.as_mut(), u.as_ref());
    ntt_op.backward(ub.as_mut());

    // sample error
    rlwe_out.iter_rows_mut().for_each(|ri| {
        RandomFillGaussianInModulus::random_fill(rng, &q, ri.as_mut());
    });

    // a*u + e0
    mod_op.elwise_add_mut(rlwe_out.get_row_mut(0), ua.as_ref());
    // b*u + e1
    mod_op.elwise_add_mut(rlwe_out.get_row_mut(1), ub.as_ref());

    // b*u + e1 + m
    mod_op.elwise_add_mut(rlwe_out.get_row_mut(1), m);
}

/// Returns RLWE public key generated using RLWE secret key
pub(crate) fn rlwe_public_key<
    Ro: RowMut + RowEntity,
    S,
    ModOp: VectorOps<Element = Ro::Element> + GetModulus<Element = Ro::Element>,
    NttOp: Ntt<Element = Ro::Element>,
    PRng: RandomFillUniformInModulus<[Ro::Element], ModOp::M>,
    Rng: RandomFillGaussianInModulus<[Ro::Element], ModOp::M>,
>(
    part_b_out: &mut Ro,
    s: &[S],
    ntt_op: &NttOp,
    mod_op: &ModOp,
    p_rng: &mut PRng,
    rng: &mut Rng,
) where
    Ro: TryConvertFrom1<[S], ModOp::M>,
{
    let ring_size = s.len();
    assert!(part_b_out.as_ref().len() == ring_size);

    let q = mod_op.modulus();

    // sample a
    let mut a = {
        let mut tmp = Ro::zeros(ring_size);
        RandomFillUniformInModulus::random_fill(p_rng, &q, tmp.as_mut());
        tmp
    };
    ntt_op.forward(a.as_mut());

    // s*a
    let mut sa = Ro::try_convert_from(s, &q);
    ntt_op.forward(sa.as_mut());
    mod_op.elwise_mul_mut(sa.as_mut(), a.as_ref());
    ntt_op.backward(sa.as_mut());

    // s*a + e
    RandomFillGaussianInModulus::random_fill(rng, &q, part_b_out.as_mut());
    mod_op.elwise_add_mut(part_b_out.as_mut(), sa.as_ref());
}

/// Decrypts ciphertext RLWE(m) and returns noisy m
///
/// We assume RLWE(m) = [a, b] is a degree 1 ciphertext s.t. b - sa = e + m
pub(crate) fn decrypt_rlwe<
    R: RowMut,
    M: Matrix<MatElement = R::Element>,
    ModOp: VectorOps<Element = R::Element> + GetModulus<Element = R::Element>,
    NttOp: Ntt<Element = R::Element>,
    S,
>(
    rlwe_ct: &M,
    s: &[S],
    m_out: &mut R,
    ntt_op: &NttOp,
    mod_op: &ModOp,
) where
    R: TryConvertFrom1<[S], ModOp::M>,
    R::Element: Copy,
{
    let ring_size = s.len();
    assert!(rlwe_ct.dimension() == (2, ring_size));
    assert!(m_out.as_ref().len() == ring_size);

    // transform a to evluation form
    m_out.as_mut().copy_from_slice(rlwe_ct.get_row_slice(0));
    ntt_op.forward(m_out.as_mut());

    // -s*a
    let mut s = R::try_convert_from(&s, mod_op.modulus());
    ntt_op.forward(s.as_mut());
    mod_op.elwise_mul_mut(m_out.as_mut(), s.as_ref());
    mod_op.elwise_neg_mut(m_out.as_mut());
    ntt_op.backward(m_out.as_mut());

    // m+e = b - s*a
    mod_op.elwise_add_mut(m_out.as_mut(), rlwe_ct.get_row_slice(1));
}

// Measures maximum noise in degree 1 RLWE ciphertext against message `want_m`
fn measure_max_noise<
    Mmut: MatrixMut + Matrix,
    ModOp: VectorOps<Element = Mmut::MatElement> + GetModulus<Element = Mmut::MatElement>,
    NttOp: Ntt<Element = Mmut::MatElement>,
    S,
>(
    rlwe_ct: &Mmut,
    want_m: &Mmut::R,
    ntt_op: &NttOp,
    mod_op: &ModOp,
    s: &[S],
) -> f64
where
    <Mmut as Matrix>::R: RowMut,
    Mmut::R: RowEntity + TryConvertFrom1<[S], ModOp::M>,
    Mmut::MatElement: PrimInt + ToPrimitive + Debug,
{
    let ring_size = s.len();
    assert!(rlwe_ct.dimension() == (2, ring_size));
    assert!(want_m.as_ref().len() == ring_size);

    // -(s * a)
    let q = mod_op.modulus();
    let mut s = Mmut::R::try_convert_from(s, &q);
    ntt_op.forward(s.as_mut());
    let mut a = Mmut::R::zeros(ring_size);
    a.as_mut().copy_from_slice(rlwe_ct.get_row_slice(0));
    ntt_op.forward(a.as_mut());
    mod_op.elwise_mul_mut(s.as_mut(), a.as_ref());
    mod_op.elwise_neg_mut(s.as_mut());
    ntt_op.backward(s.as_mut());

    // m+e = b - s*a
    let mut m_plus_e = s;
    mod_op.elwise_add_mut(m_plus_e.as_mut(), rlwe_ct.get_row_slice(1));

    // difference
    mod_op.elwise_sub_mut(m_plus_e.as_mut(), want_m.as_ref());

    let mut max_diff_bits = f64::MIN;
    m_plus_e.as_ref().iter().for_each(|v| {
        let bits = (q.map_element_to_i64(v).to_f64().unwrap().abs()).log2();

        if max_diff_bits < bits {
            max_diff_bits = bits;
        }
    });

    return max_diff_bits;
}
