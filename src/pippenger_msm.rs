use std::marker::PhantomData;
use std::ops::AddAssign;

use ark_ec::short_weierstrass_jacobian::GroupAffine;
use ark_ff::prelude::*;
use ark_std::vec::Vec;
use ark_ec::{AffineCurve, ProjectiveCurve, short_weierstrass_jacobian::GroupProjective};

/// The result of this function is only approximately `ln(a)`
/// [`Explanation of usage`]
///
/// [`Explanation of usage`]: https://github.com/scipr-lab/zexe/issues/79#issue-556220473
fn ln_without_floats(a: usize) -> usize {
    // log2(a) * ln(2)
    (ark_std::log2(a) * 69 / 100) as usize
}

pub struct VariableBaseMSM;

impl VariableBaseMSM {
    pub fn multi_scalar_mul<G: AffineCurve>(
        bases: &[G],
        scalars: &[<G::ScalarField as PrimeField>::BigInt],
    ) -> G::Projective {
        let size = ark_std::cmp::min(bases.len(), scalars.len());
        let scalars = &scalars[..size];
        let bases = &bases[..size];
        let scalars_and_bases_iter = scalars.iter().zip(bases).filter(|(s, _)| !s.is_zero());

        let c = if size < 32 {
            3
        } else {
            ln_without_floats(size) + 2
        };

        let num_bits = <G::ScalarField as PrimeField>::Params::MODULUS_BITS as usize;
        let fr_one = G::ScalarField::one().into_repr();

        let zero = G::Projective::zero();
        let window_starts: Vec<_> = (0..num_bits).step_by(c).collect();

        // Each window is of size `c`.
        // We divide up the bits 0..num_bits into windows of size `c`, and
        // in parallel process each such window.
        let window_sums: Vec<_> = window_starts.into_iter()
            .map(|w_start| {
                let mut res = zero;
                // We don't need the "zero" bucket, so we only have 2^c - 1 buckets.
                let mut buckets = vec![zero; (1 << c) - 1];
                // This clone is cheap, because the iterator contains just a
                // pointer and an index into the original vectors.
                scalars_and_bases_iter.clone().for_each(|(&scalar, base)| {
                    if scalar == fr_one {
                        // We only process unit scalars once in the first window.
                        if w_start == 0 {
                            res.add_assign_mixed(base);
                        }
                    } else {
                        let mut scalar = scalar;

                        // We right-shift by w_start, thus getting rid of the
                        // lower bits.
                        scalar.divn(w_start as u32);

                        // We mod the remaining bits by 2^{window size}, thus taking `c` bits.
                        let scalar = scalar.as_ref()[0] % (1 << c);

                        // If the scalar is non-zero, we update the corresponding
                        // bucket.
                        // (Recall that `buckets` doesn't have a zero bucket.)
                        if scalar != 0 {
                            buckets[(scalar - 1) as usize].add_assign_mixed(base);
                        }
                    }
                });

                // Compute sum_{i in 0..num_buckets} (sum_{j in i..num_buckets} bucket[j])
                // This is computed below for b buckets, using 2b curve additions.
                //
                // We could first normalize `buckets` and then use mixed-addition
                // here, but that's slower for the kinds of groups we care about
                // (Short Weierstrass curves and Twisted Edwards curves).
                // In the case of Short Weierstrass curves,
                // mixed addition saves ~4 field multiplications per addition.
                // However normalization (with the inversion batched) takes ~6
                // field multiplications per element,
                // hence batch normalization is a slowdown.

                // `running_sum` = sum_{j in i..num_buckets} bucket[j],
                // where we iterate backward from i = num_buckets to 0.
                let mut running_sum = G::Projective::zero();
                buckets.into_iter().rev().for_each(|b| {
                    running_sum += &b;
                    res += &running_sum;
                });
                res
            })
            .collect();

        // We store the sum for the lowest window.
        let lowest = *window_sums.first().unwrap();

        // We're traversing windows from high to low.
        lowest
            + &window_sums[1..]
                .iter()
                .rev()
                .fold(zero, |mut total, sum_i| {
                    total += sum_i;
                    for _ in 0..c {
                        total.double_in_place();
                    }
                    total
                })
    }

    /// Independent point addition with the mixed addition algorithm.
    /// For example, for index i, we computes points[first_index_vec[i]] + points[second_index_vec[i]].
    pub fn mixed_point_addition<G: AffineCurve>(
        points: &[G],
        first_index_vec: &[usize],
        second_index_vec: &[usize],
    ) -> Vec<G::Projective> {
        assert_eq!(first_index_vec.len(), second_index_vec.len());

        // Check out-of-boundary error
        // Assume no NaN in first_index_vec and second_index_vec
        let max_idx = first_index_vec.iter().max().unwrap();
        assert!(*max_idx < points.len());
        let max_idx = second_index_vec.iter().max().unwrap();
        assert!(*max_idx < points.len());

        let zero = G::Projective::zero();
        let mut results = vec![zero; first_index_vec.len()];

        for i in 0..first_index_vec.len() {
            let first_idx = first_index_vec[i];
            let second_idx = second_index_vec[i];
            let mut res = points[first_idx].into_projective();
            res.add_assign_mixed(&points[second_idx]);
            results[i] = res;
        }
        results
    }

    // Independent point addition with batch affine optimization.
    // For index i, we computes points[first_index_vec[i]] + points[second_index_vec[i]].
    // For detailed comparison against `fn mixed_point_addition(...)`, 
    //     please check doc at https://hackmd.io/@tazAymRSQCGXTUKkbh1BAg/Sk27liTW9
    pub fn batch_affine_point_addition<G: AffineCurve>(
        points: &[G],
        first_index_vec: &[usize],
        second_index_vec: &[usize],
    ) -> Vec<G::Projective> {
        assert_eq!(first_index_vec.len(), second_index_vec.len());

        // Check out-of-boundary error
        // Assume no NaN in first_index_vec and second_index_vec
        let max_idx = first_index_vec.iter().max().unwrap();
        assert!(*max_idx < points.len());
        let max_idx = second_index_vec.iter().max().unwrap();
        assert!(*max_idx < points.len());

        let size = first_index_vec.len();

        // A collection of a_i = x_{i,2} - x_{i,1}
        let mut a_vec = vec![G::BaseField::zero(); size];
        let mut d_vec = vec![G::BaseField::one(); size];

        for i in 0..size {
            let first_idx = first_index_vec[i];
            let second_idx = second_index_vec[i];
            a_vec[i] = points[second_idx].x - points[first_idx].x;
        }

        for i in 1..size {
            d_vec[i] = d_vec[i-1]*a_vec[i-1];
        }
        let s = (d_vec[size-1] * a_vec[size-1]).inverse().unwrap();

        let mut e_vec = vec![G::BaseField::zero(); size];
        e_vec[size-1] = s;
        for i in (0..size-1).rev() {
            e_vec[i] = e_vec[i+1]*a_vec[i+1];
        }

        let mut r_vec = vec![G::BaseField::zero(); size];
        let zero = G::Projective::zero();
        let result = vec![zero; size];
        for i in 0..size {
            // r_vec[i] = 1/(x_{i,2} - x_{i,1})
            r_vec[i] = d_vec[i] * e_vec[i];

            let first_idx = first_index_vec[i];
            let second_idx = second_index_vec[i];
            let first_point = points[first_idx];
            let second_point = points[second_idx];

            let m = (second_point.y - first_point.y) * r_vec[i];
            let x3 = m*m - first_point.x - second_point.x;
            let y3 = first_point.x + m * (x3 - first_point.x);

            let output_point = GroupAffine{
                x: x3,
                y: y3,
                infinity: false,
                _params: PhantomData,
            };

            result[i] = output_point.into();
        }

        result
    }


}
