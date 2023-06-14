use crate::{
    arithmetic::{eval_polynomial, CurveAffine},
    circuit::{layouter::SyncDeps, Value},
    plonk::{
        sealed, Advice, Any, Assigned, Assignment, Challenge, Circuit, Column, ConstraintSystem,
        Error, Fixed, FloorPlanner, Instance, ProvingKey, Selector, VerifyingKey,
    },
    poly::batch_invert_assigned,
    poly::{
        self,
        commitment::{Blind, CommitmentScheme, Params, Prover},
        Basis, Coeff, ExtendedLagrangeCoeff, LagrangeCoeff, Polynomial, ProverQuery,
    },
    transcript::{EncodedChallenge, TranscriptWrite},
};
use ff::{Field, FromUniformBytes, PrimeField, WithSmallOrderMulGroup};
use group::{prime::PrimeCurveAffine, Curve};
use rand_core::RngCore;
use std::{
    collections::{BTreeSet, HashMap},
    ops::RangeTo,
};

/// Returns an empty (zero) polynomial in the Lagrange coefficient basis
fn empty_lagrange<F: Field>(n: usize) -> Polynomial<F, LagrangeCoeff> {
    Polynomial {
        values: vec![F::ZERO; n],
        _marker: std::marker::PhantomData,
    }
}

/// Returns an empty (zero) polynomial in the Lagrange coefficient basis, with
/// deferred inversions.
fn empty_lagrange_assigned<F: Field>(n: usize) -> Polynomial<Assigned<F>, LagrangeCoeff> {
    Polynomial {
        values: vec![F::ZERO.into(); n],
        _marker: std::marker::PhantomData,
    }
}

/// This creates a proof for the provided `circuit` when given the public
/// parameters `params` and the proving key [`ProvingKey`] that was
/// generated previously for the same circuit. The provided `instances`
/// are zero-padded internally.
pub fn create_proof<
    'params,
    Scheme: CommitmentScheme,
    P: Prover<'params, Scheme>,
    E: EncodedChallenge<Scheme::Curve>,
    R: RngCore,
    T: TranscriptWrite<Scheme::Curve, E>,
    ConcreteCircuit: Circuit<Scheme::Scalar>,
>(
    params: &'params Scheme::ParamsProver,
    vk: &VerifyingKey<Scheme::Curve>,
    circuit: ConcreteCircuit,
    instances: &[&[Scheme::Scalar]],
    mut rng: R,
    transcript: &mut T,
) -> Result<(), Error>
where
    Scheme::Scalar: WithSmallOrderMulGroup<3> + FromUniformBytes<64>,
{
    if instances.len() != vk.cs().num_instance_columns {
        return Err(Error::InvalidInstances);
    }

    // Hash verification key into transcript
    vk.hash_into(transcript)?;

    // let domain = &vk.domain;
    let mut meta = ConstraintSystem::default();

    #[cfg(feature = "circuit-params")]
    let config = ConcreteCircuit::configure_with_params(&mut meta, circuit.params());
    #[cfg(not(feature = "circuit-params"))]
    let config = ConcreteCircuit::configure(&mut meta);

    // Selector optimizations cannot be applied here; use the ConstraintSystem
    // from the verification key.
    let meta = &vk.cs();

    // generate polys for instance columns
    let instance_polys = instances
        .iter()
        .map(|values| {
            let mut poly = empty_lagrange(params.n() as usize);

            if values.len() > (poly.len() - (meta.blinding_factors() + 1)) {
                return Err(Error::InstanceTooLarge);
            }
            for (poly, value) in poly.iter_mut().zip(values.iter()) {
                // The instance is part of the transcript
                if !P::QUERY_INSTANCE {
                    transcript.common_scalar(*value)?;
                }
                *poly = *value;
            }
            Ok(poly)
        })
        .collect::<Result<Vec<_>, _>>()?;

    // For large instances, we send a commitment to it and open it with PCS
    if P::QUERY_INSTANCE {
        let instance_commitments_projective: Vec<_> = instance_polys
            .iter()
            .map(|poly| params.commit_lagrange(poly, Blind::default()))
            .collect();
        let mut instance_commitments =
            vec![Scheme::Curve::identity(); instance_commitments_projective.len()];
        <Scheme::Curve as CurveAffine>::CurveExt::batch_normalize(
            &instance_commitments_projective,
            &mut instance_commitments,
        );
        let instance_commitments = instance_commitments;
        drop(instance_commitments_projective);

        for commitment in &instance_commitments {
            transcript.common_point(*commitment)?;
        }
    }

    #[derive(Clone)]
    struct AdviceSingle<C: CurveAffine, B: Basis> {
        pub advice_polys: Vec<Polynomial<C::Scalar, B>>,
        pub advice_blinds: Vec<Blind<C::Scalar>>,
    }

    struct WitnessCollection<'a, F: Field> {
        k: u32,
        current_phase: sealed::Phase,
        advice: Vec<Polynomial<Assigned<F>, LagrangeCoeff>>,
        challenges: &'a HashMap<usize, F>,
        instances: &'a [&'a [F]],
        usable_rows: RangeTo<usize>,
        _marker: std::marker::PhantomData<F>,
    }

    impl<'a, F: Field> SyncDeps for WitnessCollection<'a, F> {}

    impl<'a, F: Field> Assignment<F> for WitnessCollection<'a, F> {
        fn enter_region<NR, N>(&mut self, _: N)
        where
            NR: Into<String>,
            N: FnOnce() -> NR,
        {
            // Do nothing; we don't care about regions in this context.
        }

        fn exit_region(&mut self) {
            // Do nothing; we don't care about regions in this context.
        }

        fn enable_selector<A, AR>(&mut self, _: A, _: &Selector, _: usize) -> Result<(), Error>
        where
            A: FnOnce() -> AR,
            AR: Into<String>,
        {
            // We only care about advice columns here

            Ok(())
        }

        fn annotate_column<A, AR>(&mut self, _annotation: A, _column: Column<Any>)
        where
            A: FnOnce() -> AR,
            AR: Into<String>,
        {
            // Do nothing
        }

        fn query_instance(&self, column: Column<Instance>, row: usize) -> Result<Value<F>, Error> {
            if !self.usable_rows.contains(&row) {
                return Err(Error::not_enough_rows_available(self.k));
            }

            self.instances
                .get(column.index())
                .and_then(|column| column.get(row))
                .map(|v| Value::known(*v))
                .ok_or(Error::BoundsFailure)
        }

        fn assign_advice<V, VR, A, AR>(
            &mut self,
            _: A,
            column: Column<Advice>,
            row: usize,
            to: V,
        ) -> Result<(), Error>
        where
            V: FnOnce() -> Value<VR>,
            VR: Into<Assigned<F>>,
            A: FnOnce() -> AR,
            AR: Into<String>,
        {
            // Ignore assignment of advice column in different phase than current one.
            if self.current_phase != column.column_type().phase {
                return Ok(());
            }

            if !self.usable_rows.contains(&row) {
                return Err(Error::not_enough_rows_available(self.k));
            }

            *self
                .advice
                .get_mut(column.index())
                .and_then(|v| v.get_mut(row))
                .ok_or(Error::BoundsFailure)? = to().into_field().assign()?;

            Ok(())
        }

        fn assign_fixed<V, VR, A, AR>(
            &mut self,
            _: A,
            _: Column<Fixed>,
            _: usize,
            _: V,
        ) -> Result<(), Error>
        where
            V: FnOnce() -> Value<VR>,
            VR: Into<Assigned<F>>,
            A: FnOnce() -> AR,
            AR: Into<String>,
        {
            // We only care about advice columns here

            Ok(())
        }

        fn copy(
            &mut self,
            _: Column<Any>,
            _: usize,
            _: Column<Any>,
            _: usize,
        ) -> Result<(), Error> {
            // We only care about advice columns here

            Ok(())
        }

        fn fill_from_row(
            &mut self,
            _: Column<Fixed>,
            _: usize,
            _: Value<Assigned<F>>,
        ) -> Result<(), Error> {
            Ok(())
        }

        fn get_challenge(&self, challenge: Challenge) -> Value<F> {
            self.challenges
                .get(&challenge.index())
                .cloned()
                .map(Value::known)
                .unwrap_or_else(Value::unknown)
        }

        fn push_namespace<NR, N>(&mut self, _: N)
        where
            NR: Into<String>,
            N: FnOnce() -> NR,
        {
            // Do nothing; we don't care about namespaces in this context.
        }

        fn pop_namespace(&mut self, _: Option<String>) {
            // Do nothing; we don't care about namespaces in this context.
        }
    }

    let (advice, challenges) = {
        let mut advice = AdviceSingle::<Scheme::Curve, LagrangeCoeff> {
            advice_polys: vec![empty_lagrange(params.n() as usize); meta.num_advice_columns],
            advice_blinds: vec![Blind::default(); meta.num_advice_columns],
        };
        let mut challenges = HashMap::<usize, Scheme::Scalar>::with_capacity(meta.num_challenges);

        let unusable_rows_start = params.n() as usize - (meta.blinding_factors() + 1);
        for current_phase in vk.cs().phases() {
            let column_indices = meta
                .advice_column_phase
                .iter()
                .enumerate()
                .filter_map(|(column_index, phase)| {
                    if current_phase == *phase {
                        Some(column_index)
                    } else {
                        None
                    }
                })
                .collect::<BTreeSet<_>>();

            let mut witness = WitnessCollection {
                k: params.k(),
                current_phase,
                // Seems inefficient to recreate all this data
                advice: vec![empty_lagrange_assigned(params.n() as usize); meta.num_advice_columns],
                instances,
                challenges: &challenges,
                // The prover will not be allowed to assign values to advice
                // cells that exist within inactive rows, which include some
                // number of blinding factors and an extra row for use in the
                // permutation argument.
                usable_rows: ..unusable_rows_start,
                _marker: std::marker::PhantomData,
            };

            // Synthesize the circuit to obtain the witness and other information.
            ConcreteCircuit::FloorPlanner::synthesize(
                &mut witness,
                &circuit,
                config.clone(),
                meta.constants.clone(),
            )?;

            let mut advice_values = batch_invert_assigned::<Scheme::Scalar>(
                witness
                    .advice
                    .into_iter()
                    .enumerate()
                    .filter_map(|(column_index, advice)| {
                        if column_indices.contains(&column_index) {
                            Some(advice)
                        } else {
                            None
                        }
                    })
                    .collect(),
            );

            // Add blinding factors to advice columns
            for advice_values in &mut advice_values {
                for cell in &mut advice_values[unusable_rows_start..] {
                    *cell = Scheme::Scalar::random(&mut rng);
                }
            }

            // Compute commitments to advice column polynomials
            let blinds: Vec<_> = advice_values
                .iter()
                .map(|_| Blind(Scheme::Scalar::random(&mut rng)))
                .collect();
            let advice_commitments_projective: Vec<_> = advice_values
                .iter()
                .zip(blinds.iter())
                .map(|(poly, blind)| params.commit_lagrange(poly, *blind))
                .collect();
            let mut advice_commitments =
                vec![Scheme::Curve::identity(); advice_commitments_projective.len()];
            <Scheme::Curve as CurveAffine>::CurveExt::batch_normalize(
                &advice_commitments_projective,
                &mut advice_commitments,
            );
            let advice_commitments = advice_commitments;
            drop(advice_commitments_projective);

            for commitment in &advice_commitments {
                transcript.write_point(*commitment)?;
            }
            for ((column_index, advice_values), blind) in
                column_indices.iter().zip(advice_values).zip(blinds)
            {
                advice.advice_polys[*column_index] = advice_values;
                advice.advice_blinds[*column_index] = blind;
            }

            for (index, phase) in meta.challenge_phase.iter().enumerate() {
                if current_phase == *phase {
                    let existing =
                        challenges.insert(index, *transcript.squeeze_challenge_scalar::<()>());
                    assert!(existing.is_none());
                }
            }
        }

        assert_eq!(challenges.len(), meta.num_challenges);
        let challenges = (0..meta.num_challenges)
            .map(|index| challenges.remove(&index).unwrap())
            .collect::<Vec<_>>();

        (advice, challenges)
    };
    Ok(())
}
