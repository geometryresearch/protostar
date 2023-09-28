use ff::{Field, FromUniformBytes, WithSmallOrderMulGroup};
use group::Curve;
use halo2curves::{CurveAffine, CurveExt};
use rand_core::RngCore;
use std::{
    collections::{BTreeSet, HashMap},
    iter::zip,
};

use super::{accumulator::Accumulator, keygen::ProvingKey};
use crate::arithmetic::{
    best_multiexp, compute_inner_product, eval_polynomial, parallelize, powers,
};
use crate::plonk::Error;
use crate::poly::commitment::{CommitmentScheme, Verifier};
use crate::poly::VerificationStrategy;
use crate::poly::{
    commitment::{Blind, Params, MSM},
    Guard, VerifierQuery,
};
use crate::transcript::{
    read_n_points, read_n_scalars, EncodedChallenge, TranscriptRead, TranscriptWrite,
};
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

#[derive(Debug, Clone, PartialEq)]
pub struct LookupAccumulator<C: CurveAffine> {
    pub m: C,
    pub r: C::Scalar,
    pub thetas: Vec<C::Scalar>,
    pub g: C,
    pub h: C,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerifierAccumulator<C: CurveAffine> {
    pub instance: Vec<Vec<C::Scalar>>,
    pub advice: Vec<C>,
    pub challenges: Vec<C::Scalar>,
    pub lookup_accumulators: Vec<LookupAccumulator<C>>,
    pub beta: C::Scalar,
    pub beta_commitment: C,
    pub beta_error: C,
    pub ys: Vec<C::Scalar>,
    pub error: C::Scalar,
}

impl<C: CurveAffine> VerifierAccumulator<C> {
    /// Create a new `VerifierAccumulator` by reading the IOP transcripts from the Prover and save commitments and challenges
    pub fn new_from_prover<E: EncodedChallenge<C>, T: TranscriptRead<C, E>>(
        transcript: &mut T,
        instances: &[&[C::Scalar]],
        // TODO(@gnosed): replace pk with vk: VerifiyingKey<C>
        pk: &ProvingKey<C>,
    ) -> Result<Self, Error> {
        //
        // Get instance commitments
        //
        // Check that instances matches the expected number of instance columns
        if instances.len() != pk.cs.num_instance_columns {
            return Err(Error::InvalidInstances);
        }

        for instance in instances.iter() {
            for value in instance.iter() {
                transcript.common_scalar(*value)?;
            }
        }

        let instance: Vec<_> = instances
            .into_iter()
            .map(|instance| instance.to_vec())
            .collect();

        // Hash verification key into transcript
        // TODO(@gnosed): is it necessary? If yes, change it when the VerifyingKey was implemented
        // vk.hash_into(transcript)?;

        // for instance in instances.iter() {
        //     for instance in instance.iter() {
        //         for value in instance.iter() {
        //             transcript.common_scalar(*value)?;
        //         }
        //     }
        // }

        //
        // Get advice commitments and challenges
        //
        let (advice, challenges) = {
            let mut advice_commitments = vec![C::identity(); pk.cs.num_advice_columns];
            let mut challenges = vec![C::Scalar::ZERO; pk.cs.num_challenges];

            for current_phase in pk.cs.phases() {
                for (phase, commitment) in pk
                    .cs
                    .advice_column_phase
                    .iter()
                    .zip(advice_commitments.iter_mut())
                {
                    if current_phase == *phase {
                        *commitment = transcript.read_point()?;
                    }
                }
                for (phase, challenge) in pk.cs.challenge_phase.iter().zip(challenges.iter_mut()) {
                    if current_phase == *phase {
                        *challenge = *transcript.squeeze_challenge_scalar::<()>();
                    }
                }
            }

            (advice_commitments, challenges)
        };
        //
        // Get lookup commitments to m(x), g(x) and h(x) polys
        //
        let num_lookups = pk.cs.lookups.len();

        // Read all commitments m_i(X)
        let mut m_commitments = vec![C::identity(); num_lookups];
        for m_commitment in m_commitments.iter_mut() {
            *m_commitment = transcript.read_point()?;
        }

        // Get challenge r, theta
        let [r, theta] = [(); 2].map(|_| *transcript.squeeze_challenge_scalar::<C::Scalar>());

        // Get h_i(X), g_i(X) from the transcript for each lookup
        let lookup_accumulators: Vec<_> = pk
            .cs
            .lookups
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let num_thetas = arg.input_expressions().len();
                let thetas: Vec<_> = powers(theta).take(num_thetas).collect();
                let g_commitment = transcript.read_point()?;
                let h_commitment = transcript.read_point()?;
                Ok(LookupAccumulator {
                    m: m_commitments[i],
                    r,
                    thetas,
                    g: g_commitment,
                    h: h_commitment,
                })
            })
            .collect::<Result<Vec<LookupAccumulator<C>>, Error>>()?;

        //
        // Get beta commitment
        //
        let beta = *transcript.squeeze_challenge_scalar::<C::Scalar>();
        let beta_commitment = transcript.read_point()?;
        let beta_error = C::identity();

        // Challenge for the RLC of all constraints (all gates and all lookups)
        let y = *transcript.squeeze_challenge_scalar::<C::Scalar>();
        let ys: Vec<C::Scalar> = powers(y).take(pk.num_folding_constraints()).collect();

        Ok(VerifierAccumulator {
            instance,
            advice,
            challenges,
            beta,
            beta_commitment,
            beta_error,
            lookup_accumulators,
            ys,
            error: C::Scalar::ZERO,
        })
    }
    pub fn fold<E: EncodedChallenge<C>, T: TranscriptRead<C, E>>(
        &mut self,
        acc1: &VerifierAccumulator<C>,
        pk: &ProvingKey<C>,
        transcript: &mut T,
    ) {
        //
        // Get error commitments
        // (We subtract 2 since we expect the quotient of the error polynomial)
        //
        let final_error_poly_len = pk.max_folding_constraints_degree() + 1;
        // Prover doesn't send the first two coefficient since Verifier already know e(0) and e(1)
        let quotient_final_error_poly_len = final_error_poly_len - 2;

        let mut e_commitments = vec![C::Scalar::ZERO; quotient_final_error_poly_len];
        for e_commitment in e_commitments.iter_mut() {
            *e_commitment = transcript.read_scalar().unwrap();
        }
        let alpha = *transcript.squeeze_challenge_scalar::<C::Scalar>();

        // eval e'(alpha), then eval e(alpha) = (1-alpha)*alpha*e'(alpha) + (1-alpha)*e(0) + alpha*e(1)
        let quotient_final_error_poly = eval_polynomial(&e_commitments, alpha);
        let final_error = alpha * (C::Scalar::ONE - alpha) * quotient_final_error_poly
            + (C::Scalar::ONE - alpha) * self.error
            + alpha * acc1.error;
        self.error = final_error;

        // Fold instances
        for (instance0, instance1) in zip(self.instance.iter_mut(), acc1.instance.iter()) {
            for (i0, i1) in zip(instance0.iter_mut(), instance1.iter()) {
                *i0 = (*i1 - *i0) * i1 + *i0;
            }
        }

        // Fold all commitments
        fn fold_commitments<C: CurveAffine>(
            self_commitments: &mut Vec<C>,
            acc1_commitments: &Vec<C>,
            alpha: C::Scalar,
        ) {
            for (self_c, acc1_c) in zip(self_commitments.iter_mut(), acc1_commitments.iter()) {
                *self_c = ((*acc1_c - *self_c) * alpha + *self_c).to_affine();
            }
        }

        fold_commitments(&mut self.advice, &acc1.advice, alpha);

        for (self_lookup, acc1_lookup) in zip(
            self.lookup_accumulators.iter_mut(),
            acc1.lookup_accumulators.iter(),
        ) {
            self_lookup.m = ((acc1_lookup.m - self_lookup.m) * alpha + self_lookup.m).to_affine();
            self_lookup.g = ((acc1_lookup.g - self_lookup.g) * alpha + self_lookup.g).to_affine();
            self_lookup.h = ((acc1_lookup.h - self_lookup.h) * alpha + self_lookup.h).to_affine();

            self_lookup.r = (acc1_lookup.r - self_lookup.r) * alpha + self_lookup.r;

            for (theta0, theta1) in zip(self_lookup.thetas.iter_mut(), acc1_lookup.thetas.iter()) {
                *theta0 = (*theta1 - *theta0) * alpha + *theta0;
            }
        }

        // Compute commitment to error vector for beta commitment correctness
        self.beta_error = {
            let error0 = self.beta_error;
            let error1 = acc1.beta_error;
            let beta_com0 = self.beta_commitment;
            let beta_com1 = acc1.beta_commitment;
            let beta0 = self.beta;
            let beta1 = acc1.beta;
            let error_quotient = beta_com0 * (beta1 - beta0) + beta_com1 * (beta0 - beta1);
            (error0 * (C::Scalar::ONE - alpha)
                + error1 * C::Scalar::ONE
                + error_quotient * ((C::Scalar::ONE - alpha) * alpha))
                .to_affine()
        };

        // fold beta challenge
        self.beta = (acc1.beta - self.beta) * alpha
            + self.beta;

            self.beta_commitment = ((acc1.beta_commitment - self.beta_commitment) * alpha
            + self.beta_commitment)
            .to_affine();

        // Fold all challenges
        for (self_c, acc1_c) in zip(self.challenges.iter_mut(), acc1.challenges.iter()) {
            *self_c = (*acc1_c - *self_c) * alpha + *self_c;
        }
        // fold ys challenges
        for (y0, y1) in zip(self.ys.iter_mut(), acc1.ys.iter()) {
            *y0 = (*y1 - *y0) * alpha + *y0;
        }
    }
}

#[cfg(test)]
mod tests {
    use ff::{BatchInvert, FromUniformBytes, PrimeField, PrimeFieldBits};

    use crate::{
        arithmetic::{CurveAffine, Field},
        circuit::{floor_planner::V1, AssignedCell, Layouter, Value},
        dev::{metadata, FailureLocation, MockProver, VerifyFailure},
        plonk::*,
        poly::Rotation,
        poly::{
            self,
            commitment::ParamsProver,
            ipa::{
                commitment::{IPACommitmentScheme, ParamsIPA},
                multiopen::{ProverIPA, VerifierIPA},
            },
            VerificationStrategy,
        },
        protostar,
        protostar::accumulator::Accumulator,
        protostar::verifier::{LookupAccumulator, VerifierAccumulator},
        transcript::{
            Blake2bRead, Blake2bWrite, Challenge255, TranscriptReadBuffer, TranscriptWriterBuffer,
        },
    };

    use halo2curves::pasta::{self, pallas, Fp};
    use rand_core::{OsRng, RngCore};
    use std::{
        iter::{self, zip},
        marker::PhantomData,
    };

    fn rand_2d_array<F: Field, R: RngCore, const W: usize, const H: usize>(
        rng: &mut R,
    ) -> [[F; H]; W] {
        [(); W].map(|_| [(); H].map(|_| F::random(&mut *rng)))
    }

    fn shuffled<F: Field, R: RngCore, const W: usize, const H: usize>(
        original: [[F; H]; W],
        rng: &mut R,
    ) -> [[F; H]; W] {
        let mut shuffled = original;

        for row in (1..H).rev() {
            let rand_row = (rng.next_u32() as usize) % row;
            for column in shuffled.iter_mut() {
                column.swap(row, rand_row);
            }
        }

        shuffled
    }

    #[derive(Clone)]
    pub struct MyConfig<const W: usize> {
        q_shuffle: Selector,
        q_first: Selector,
        q_last: Selector,
        original: [Column<Advice>; W],
        shuffled: [Column<Advice>; W],
        theta: Challenge,
        gamma: Challenge,
        z: Column<Advice>,
    }

    impl<const W: usize> MyConfig<W> {
        fn configure<F: Field>(meta: &mut ConstraintSystem<F>) -> Self {
            let [q_shuffle, q_first, q_last] = [(); 3].map(|_| meta.selector());
            // First phase
            let original = [(); W].map(|_| meta.advice_column_in(FirstPhase));
            let shuffled = [(); W].map(|_| meta.advice_column_in(FirstPhase));
            let [theta, gamma] = [(); 2].map(|_| meta.challenge_usable_after(FirstPhase));
            // Second phase
            let z = meta.advice_column_in(SecondPhase);

            meta.create_gate("z should start with 1", |_| {
                let one = Expression::Constant(F::ONE);

                vec![q_first.expr() * (one - z.cur())]
            });

            meta.create_gate("z should end with 1", |_| {
                let one = Expression::Constant(F::ONE);

                vec![q_last.expr() * (one - z.cur())]
            });

            meta.create_gate("z should have valid transition", |_| {
                let q_shuffle = q_shuffle.expr();
                let original = original.map(|advice| advice.cur());
                let shuffled = shuffled.map(|advice| advice.cur());
                let [theta, gamma] = [theta, gamma].map(|challenge| challenge.expr());

                // Compress
                let original = original
                    .iter()
                    .cloned()
                    .reduce(|acc, a| acc * theta.clone() + a)
                    .unwrap();
                let shuffled = shuffled
                    .iter()
                    .cloned()
                    .reduce(|acc, a| acc * theta.clone() + a)
                    .unwrap();

                vec![
                    q_shuffle
                        * (z.cur() * (original + gamma.clone()) - z.next() * (shuffled + gamma)),
                ]
            });

            Self {
                q_shuffle,
                q_first,
                q_last,
                original,
                shuffled,
                theta,
                gamma,
                z,
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct MyCircuit<F: Field, const W: usize, const H: usize> {
        original: Value<[[F; H]; W]>,
        shuffled: Value<[[F; H]; W]>,
    }

    impl<F: Field, const W: usize, const H: usize> MyCircuit<F, W, H> {
        pub fn rand<R: RngCore>(rng: &mut R) -> Self {
            let original = rand_2d_array::<F, _, W, H>(rng);
            let shuffled = shuffled(original, rng);

            Self {
                original: Value::known(original),
                shuffled: Value::known(shuffled),
            }
        }
    }

    impl<F: Field, const W: usize, const H: usize> Circuit<F> for MyCircuit<F, W, H> {
        type Config = MyConfig<W>;
        type FloorPlanner = V1;
        #[cfg(feature = "circuit-params")]
        type Params = ();

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            MyConfig::configure(meta)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            let theta = layouter.get_challenge(config.theta);
            let gamma = layouter.get_challenge(config.gamma);

            layouter.assign_region(
                || "Shuffle original into shuffled",
                |mut region| {
                    // Keygen
                    config.q_first.enable(&mut region, 0)?;
                    config.q_last.enable(&mut region, H)?;
                    for offset in 0..H {
                        config.q_shuffle.enable(&mut region, offset)?;
                    }

                    // First phase
                    for (idx, (&column, values)) in zip(
                        config.original.iter(),
                        self.original.transpose_array().iter(),
                    )
                    .enumerate()
                    {
                        for (offset, &value) in values.transpose_array().iter().enumerate() {
                            region.assign_advice(
                                || format!("original[{}][{}]", idx, offset),
                                column,
                                offset,
                                || value,
                            )?;
                        }
                    }
                    for (idx, (&column, values)) in zip(
                        config.shuffled.iter(),
                        self.shuffled.transpose_array().iter(),
                    )
                    .enumerate()
                    {
                        for (offset, &value) in values.transpose_array().iter().enumerate() {
                            region.assign_advice(
                                || format!("shuffled[{}][{}]", idx, offset),
                                column,
                                offset,
                                || value,
                            )?;
                        }
                    }

                    // Second phase
                    let z = self.original.zip(self.shuffled).zip(theta).zip(gamma).map(
                        |(((original, shuffled), theta), gamma)| {
                            let mut product = vec![F::ZERO; H];
                            for (idx, product) in product.iter_mut().enumerate() {
                                let mut compressed = F::ZERO;
                                for value in shuffled.iter() {
                                    compressed *= theta;
                                    compressed += value[idx];
                                }

                                *product = compressed + gamma;
                            }

                            product.iter_mut().batch_invert();

                            for (idx, product) in product.iter_mut().enumerate() {
                                let mut compressed = F::ZERO;
                                for value in original.iter() {
                                    compressed *= theta;
                                    compressed += value[idx];
                                }

                                *product *= compressed + gamma;
                            }

                            #[allow(clippy::let_and_return)]
                            let z = iter::once(F::ONE)
                                .chain(product)
                                .scan(F::ONE, |state, cur| {
                                    *state *= &cur;
                                    Some(*state)
                                })
                                .collect::<Vec<_>>();

                            #[cfg(feature = "sanity-checks")]
                            assert_eq!(F::ONE, *z.last().unwrap());

                            z
                        },
                    );
                    for (offset, value) in z.transpose_vec(H + 1).into_iter().enumerate() {
                        region.assign_advice(
                            || format!("z[{}]", offset),
                            config.z,
                            offset,
                            || value,
                        )?;
                    }

                    Ok(())
                },
            )
        }
    }

    /// A lookup table of values from 0..RANGE.
    #[derive(Debug, Clone)]
    pub(super) struct RangeTableConfig<F: PrimeFieldBits, const RANGE: usize> {
        pub(super) value: TableColumn,
        _marker: PhantomData<F>,
    }

    impl<F: PrimeFieldBits, const RANGE: usize> RangeTableConfig<F, RANGE> {
        pub(super) fn configure(meta: &mut ConstraintSystem<F>) -> Self {
            let value = meta.lookup_table_column();

            Self {
                value,
                _marker: PhantomData,
            }
        }

        pub(super) fn load(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
            layouter.assign_table(
                || "load range-check table",
                |mut table| {
                    let mut offset = 0;
                    for value in 0..RANGE {
                        table.assign_cell(
                            || "num_bits",
                            self.value,
                            offset,
                            || Value::known(F::from(value as u64)),
                        )?;
                        offset += 1;
                    }

                    Ok(())
                },
            )
        }
    }

    #[derive(Debug, Clone)]
    /// A range-constrained value in the circuit produced by the RangeCheckConfig.
    struct RangeConstrained<F: PrimeFieldBits, const RANGE: usize>(AssignedCell<Assigned<F>, F>);

    #[derive(Debug, Clone)]
    struct RangeCheckConfig<F: PrimeFieldBits, const RANGE: usize, const LOOKUP_RANGE: usize> {
        q_range_check: Selector,
        q_lookup: Selector,
        value: Column<Advice>,
        table: RangeTableConfig<F, LOOKUP_RANGE>,
    }

    impl<F: PrimeFieldBits, const RANGE: usize, const LOOKUP_RANGE: usize>
        RangeCheckConfig<F, RANGE, LOOKUP_RANGE>
    {
        pub fn configure(meta: &mut ConstraintSystem<F>, value: Column<Advice>) -> Self {
            let q_range_check = meta.selector();
            let q_lookup = meta.complex_selector();
            let table = RangeTableConfig::configure(meta);

            meta.create_gate("range check", |meta| {
                //        value     |    q_range_check
                //       ------------------------------
                //          v       |         1

                let q = meta.query_selector(q_range_check);
                let value = meta.query_advice(value, Rotation::cur());

                // Given a range R and a value v, returns the expression
                // (v) * (1 - v) * (2 - v) * ... * (R - 1 - v)
                let range_check = |range: usize, value: Expression<F>| {
                    assert!(range > 0);
                    (1..range).fold(value.clone(), |expr, i| {
                        expr * (Expression::Constant(F::from(i as u64)) - value.clone())
                    })
                };

                Constraints::with_selector(q, [("range check", range_check(RANGE, value))])
            });

            meta.lookup("lookup", |meta| {
                let q_lookup = meta.query_selector(q_lookup);
                let value = meta.query_advice(value, Rotation::cur());

                vec![(q_lookup * value, table.value)]
            });

            Self {
                q_range_check,
                q_lookup,
                value,
                table,
            }
        }

        pub fn assign_simple(
            &self,
            mut layouter: impl Layouter<F>,
            value: Value<Assigned<F>>,
        ) -> Result<RangeConstrained<F, RANGE>, Error> {
            layouter.assign_region(
                || "Assign value for simple range check",
                |mut region| {
                    let offset = 0;

                    // Enable q_range_check
                    self.q_range_check.enable(&mut region, offset)?;

                    // Assign value
                    region
                        .assign_advice(|| "value", self.value, offset, || value)
                        .map(RangeConstrained)
                },
            )
        }

        pub fn assign_lookup(
            &self,
            mut layouter: impl Layouter<F>,
            value: Value<Assigned<F>>,
        ) -> Result<RangeConstrained<F, LOOKUP_RANGE>, Error> {
            layouter.assign_region(
                || "Assign value for lookup range check",
                |mut region| {
                    let offset = 0;

                    // Enable q_lookup
                    self.q_lookup.enable(&mut region, offset)?;

                    // Assign value
                    region
                        .assign_advice(|| "value", self.value, offset, || value)
                        .map(RangeConstrained)
                },
            )
        }
    }
    #[derive(Default)]
    struct RangeCheckCircuit<F: PrimeFieldBits, const RANGE: usize, const LOOKUP_RANGE: usize> {
        value: Value<Assigned<F>>,
        lookup_value: Value<Assigned<F>>,
    }

    impl<F: PrimeFieldBits, const RANGE: usize, const LOOKUP_RANGE: usize> Circuit<F>
        for RangeCheckCircuit<F, RANGE, LOOKUP_RANGE>
    {
        type Config = RangeCheckConfig<F, RANGE, LOOKUP_RANGE>;
        type FloorPlanner = V1;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let value = meta.advice_column();
            RangeCheckConfig::configure(meta, value)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            config.table.load(&mut layouter)?;

            config.assign_simple(layouter.namespace(|| "Assign simple value"), self.value)?;
            config.assign_lookup(
                layouter.namespace(|| "Assign lookup value"),
                self.lookup_value,
            )?;

            Ok(())
        }
    }

    fn check_v_and_p_transcripts<C: CurveAffine>(
        v_acc: VerifierAccumulator<C>,
        p_acc: Accumulator<C>,
    ) {
        for (col, instance) in v_acc.instance.iter().enumerate() {
            for (row, v_instance_value) in instance.iter().enumerate() {
                let p_instance_value = p_acc.gate.instance[col].values[row];
                assert_eq!(
                    *v_instance_value, p_instance_value,
                    "V and P instance at col {col} and row {row} are NOT EQUAL"
                )
            }
        }
        assert_eq!(
            p_acc
                .gate
                .advice
                .iter()
                .map(|c| c.commitment)
                .collect::<Vec<C>>(),
            v_acc.advice,
            "V and P Advice Transcripts NOT EQUAL"
        );
        assert_eq!(
            p_acc.gate.challenges, v_acc.challenges,
            "V and P Advice Challenges NOT EQUAL"
        );
        assert_eq!(
            p_acc
                .lookups
                .iter()
                .map(|v| LookupAccumulator {
                    m: v.m.commitment,
                    r: v.r,
                    thetas: v.thetas.clone(),
                    g: v.g.commitment,
                    h: v.h.commitment
                })
                .collect::<Vec<LookupAccumulator<C>>>(),
            v_acc.lookup_accumulators
        );
        assert_eq!(
            p_acc.beta.beta.values[1], v_acc.beta,
            "V and P Beta challenge NOT EQUAL"
        );
        assert_eq!(
            p_acc.beta.error.commitment, v_acc.beta_error,
            "V and P Beta error NOT EQUAL"
        );
        assert_eq!(
            p_acc.beta.beta.commitment, v_acc.beta_commitment,
            "V and P Beta Commitment NOT EQUAL"
        );
        assert_eq!(p_acc.ys, v_acc.ys, "V and P Y challenge NOT EQUAL");
        assert_eq!(p_acc.error, v_acc.error, "V and P Error NOT EQUAL");
    }

    #[test]
    fn test_one_verifier_acc() {
        let mut rng: OsRng = OsRng;

        const W: usize = 4;
        const H: usize = 32;
        const K: u32 = 8;

        let params = poly::ipa::commitment::ParamsIPA::<pallas::Affine>::new(K);

        let circuit = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);

        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        let pk = protostar::ProvingKey::new(&params, &circuit).unwrap();

        let p_acc = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();

        let proof: Vec<u8> = transcript.finalize();

        let mut v_transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
        let v_acc = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();

        check_v_and_p_transcripts(v_acc, p_acc);
    }

    #[test]
    fn test_same_acc_fold() {
        let mut rng: OsRng = OsRng;

        const W: usize = 4;
        const H: usize = 32;
        const K: u32 = 8;

        let params = poly::ipa::commitment::ParamsIPA::<pallas::Affine>::new(K);

        let circuit = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);

        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        let pk = protostar::ProvingKey::new(&params, &circuit).unwrap();

        let p_acc = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();

        let p_acc1 = Accumulator::fold(&pk, p_acc.clone(), p_acc.clone(), &mut transcript);

        let proof: Vec<u8> = transcript.finalize();

        let mut v_transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
        let v_acc = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let mut v_acc1 = v_acc.clone();

        v_acc1.fold(&v_acc, &pk, &mut v_transcript);

        check_v_and_p_transcripts(v_acc1, p_acc1);
    }

    #[test]
    fn test_two_verifier_acc() {
        let mut rng: OsRng = OsRng;

        const W: usize = 4;
        const H: usize = 32;
        const K: u32 = 8;

        let params = poly::ipa::commitment::ParamsIPA::<pallas::Affine>::new(K);

        let circuit0 = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);
        let circuit1 = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);

        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        let pk = protostar::ProvingKey::new(&params, &circuit0).unwrap();

        let acc0 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit0,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();
        let acc1 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit1,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();

        let proof: Vec<u8> = transcript.finalize();

        let mut v_transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
        let v_acc0 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let v_acc1 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();

        // Check acc0 and v_acc0 transcripts
        check_v_and_p_transcripts(v_acc0, acc0);
        // Check acc1 and v_acc1 transcripts
        check_v_and_p_transcripts(v_acc1, acc1);
    }

    #[test]
    fn test_two_verifier_acc_folding() {
        let mut rng: OsRng = OsRng;

        const W: usize = 4;
        const H: usize = 32;
        const K: u32 = 8;

        let params = poly::ipa::commitment::ParamsIPA::<pallas::Affine>::new(K);

        let circuit0 = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);
        let circuit1 = MyCircuit::<pallas::Scalar, W, H>::rand(&mut rng);

        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        let pk = protostar::ProvingKey::new(&params, &circuit0).unwrap();

        let acc0 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit0,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();
        let acc1 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit1,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();

        let acc2 = Accumulator::fold(&pk, acc0.clone(), acc1.clone(), &mut transcript);

        let proof: Vec<u8> = transcript.finalize();
        let mut v_transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);

        let v_acc0 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let v_acc1 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let mut v_acc2 = v_acc0.clone();

        v_acc2.fold(&v_acc1.clone(), &pk, &mut v_transcript);

        check_v_and_p_transcripts(v_acc0, acc0);
        check_v_and_p_transcripts(v_acc1, acc1);
        check_v_and_p_transcripts(v_acc2, acc2);
    }

    #[test]
    fn test_lookup() {
        let mut rng: OsRng = OsRng;
        const K: u32 = 9;
        const RANGE: usize = 8; // 3-bit value
        const LOOKUP_RANGE: usize = 256; // 8-bit value

        let params = poly::ipa::commitment::ParamsIPA::<pallas::Affine>::new(K);

        let circuit0 = RangeCheckCircuit::<pallas::Scalar, RANGE, LOOKUP_RANGE> {
            value: Value::known(pallas::Scalar::from(4).into()),
            lookup_value: Value::known(pallas::Scalar::from(12).into()),
        };

        let circuit1 = RangeCheckCircuit::<pallas::Scalar, RANGE, LOOKUP_RANGE> {
            value: Value::known(pallas::Scalar::from(5).into()),
            lookup_value: Value::known(pallas::Scalar::from(220).into()),
        };

        let prover0 = MockProver::run(K, &circuit0, vec![]).unwrap();
        let prover1 = MockProver::run(K, &circuit1, vec![]).unwrap();

        prover0.assert_satisfied();
        prover1.assert_satisfied();

        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        let pk = protostar::ProvingKey::new(&params, &circuit0).unwrap();

        let acc0 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit0,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();
        let acc1 = protostar::prover::create_accumulator(
            &params,
            &pk,
            &circuit1,
            &[],
            &mut rng,
            &mut transcript,
        )
        .unwrap();

        let acc2 = Accumulator::fold(&pk, acc0.clone(), acc1.clone(), &mut transcript);

        let proof: Vec<u8> = transcript.finalize();
        let mut v_transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);

        let v_acc0 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let v_acc1 = VerifierAccumulator::new_from_prover(&mut v_transcript, &[], &pk).unwrap();
        let mut v_acc2 = v_acc0.clone();

        v_acc2.fold(&v_acc1.clone(), &pk, &mut v_transcript);

        check_v_and_p_transcripts(v_acc0, acc0);
        check_v_and_p_transcripts(v_acc1, acc1);
        check_v_and_p_transcripts(v_acc2, acc2);
    }
}
